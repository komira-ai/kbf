//! The update rehearsal: a Linux host laid out by hand from release N, as a farm node is, is
//! updated to release N+1 by the real helper under the real systemd, through its local socket,
//! the way the updater agent asks for it (`install`, `switch`, then `confirm`), with N+1 fetched
//! from this repository's GitHub releases by the helper's own, unchanged fetch path.
//!
//! N+1 is the release named in `/var/tmp/kbf-update-rehearsal.to`, or the newest published
//! release when the file is absent; N is the published release before it. The host gets:
//! release N's `kbf` laid out by N's `kbf-deploy host install` under
//! `/var/lib/kbf-installer/releases`; N's `kbf-host-install` at `/usr/libexec/kbf`; the trust
//! files of N's package (`/usr/share/kbf/trust`) and its accounts; the helper's unit and the
//! updater's account from this checkout's `packaging/host-agent/linux`, the unit's
//! `--restart-unit` pointed at the one `kbf` unit here (an operator edits that line the same
//! way); and that `kbf` unit, a storage node on a `file://` cold store that runs
//! `releases/current/kbf`.
//!
//! What it proves: the helper of N, confined by the shipped unit, fetches and verifies N+1,
//! switches to it, restarts the unit onto N+1's binary, and confirms it. What it catches: any
//! of those steps failing, among them a unit that leaves the helper unable to drop to the
//! account it fetches as (`User=root` did that: every fetch failed with `Operation not
//! permitted`), a release N+1 whose files the helper of N refuses, and a switch that does not
//! leave the unit running N+1. Not covered here: the agent, the coordinator and a frontend's
//! drain and health check.
//!
//! It needs Linux, systemd, root and the network, so it is `#[ignore]`d and runs through
//! `cargo xtask sudo-test -p kbf-host-install --test update_rehearsal` on a GitHub-hosted
//! runner: in the `update-rehearsal` workflow, and in the release before the gamma stage. The
//! host and the setup it shares with the rehearsal of the agent and the coordinator
//! (`update_rehearsal_agent.rs`) are in `rehearsal/mod.rs`.

#![cfg(target_os = "linux")]

mod rehearsal;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rehearsal::{
    CHANNEL, HELPER_UNIT, KBF_UNIT, RELEASES, SOCKET, UPDATER, answers, ids_of, journal, run,
    systemctl, until,
};
use serde_json::Value;

/// Where the kbf unit's storage node listens.
const STORAGE: &str = "127.0.0.1:8983";
/// The environment variable that carries a request to [`speak_to_the_helper`].
const REQUEST_ENV: &str = "KBF_REHEARSAL_REQUEST";
/// How long one request may take: an install downloads and verifies a release.
const REQUEST_LIMIT: Duration = Duration::from_secs(900);

/// The helper's answer line, which [`speak_to_the_helper`] prints, parsed.
fn answer(text: &str) -> Value {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("answer: "))
        .unwrap_or_else(|| panic!("no answer line: {text}"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

/// Not a test of its own: the process that speaks to the helper as the updater's account. It
/// sends the request in [`REQUEST_ENV`] and prints the one line the helper answers.
#[test]
#[ignore = "run by the_helper_updates_a_hand_laid_host_from_the_last_release_to_the_next, as the updater's account"]
fn speak_to_the_helper() {
    let request = std::env::var(REQUEST_ENV).expect("a request to send");
    let mut stream = UnixStream::connect(SOCKET).expect("the helper's socket");
    stream.set_read_timeout(Some(REQUEST_LIMIT)).unwrap();
    stream.write_all(format!("{request}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    println!("answer: {}", line.trim_end());
}

/// Sends `request` to the helper as the updater's account (through `probe`, a copy of this
/// binary) and returns the answer, kept as `name`.
fn ask(probe: &Path, name: &str, request: &str) -> Value {
    let (uid, gid) = ids_of(UPDATER);
    let text = run(
        name,
        Command::new(probe)
            .args(["--ignored", "--exact", "speak_to_the_helper", "--nocapture"])
            .env_clear()
            .env(REQUEST_ENV, request)
            .current_dir("/")
            .uid(uid)
            .gid(gid),
    );
    let value = answer(&text);
    println!("{request} -> {value}");
    value
}

#[test]
#[ignore = "needs Linux, systemd, root and the network: cargo xtask sudo-test"]
fn the_helper_updates_a_hand_laid_host_from_the_last_release_to_the_next() {
    let r = rehearsal::plan();
    let (old, new, version) = (&r.from.commit, &r.to.commit, &r.version);
    rehearsal::fetch_old_release(&r);
    let _journal = rehearsal::Journal::of(&[HELPER_UNIT, KBF_UNIT]);
    let unit = rehearsal::kbf_unit("DynamicUser=yes", &format!("--storage-listen={STORAGE}"));
    rehearsal::lay_out(&r, &unit);
    systemctl("start_kbf", &["start", KBF_UNIT]);
    rehearsal::start_helper();
    until(KBF_UNIT, || answers(STORAGE));

    // The updater's account runs a copy of this binary it can reach.
    let probe = r.work.join("probe");
    fs::copy(std::env::current_exe().unwrap(), &probe).unwrap();
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();

    let before = ask(&probe, "query_before", "query");
    assert_eq!(before["result"], "query", "{before}");
    assert_eq!(
        before["commit"],
        old.as_str(),
        "the host runs release N: {before}"
    );

    let staged = ask(
        &probe,
        "install",
        &format!("install kbf={version} from {CHANNEL}"),
    );
    if staged["result"] != "staged"
        && let Some(note) = rehearsal::rate_limit_note(&journal(&[HELPER_UNIT, KBF_UNIT]))
    {
        panic!("{note} {staged}");
    }
    assert_eq!(staged["result"], "staged", "{staged}");
    assert_eq!(staged["commit"], new.as_str(), "{staged}");

    let switched = ask(&probe, "switch", &format!("switch kbf={version}"));
    assert_eq!(switched["result"], "switched", "{switched}");

    // The unit runs N+1's binary.
    let current = fs::read_link(Path::new(RELEASES).join("current")).unwrap();
    assert_eq!(current, Path::new(new), "the current link");
    let facts = run(
        "current_version",
        Command::new(Path::new(RELEASES).join("current/kbf")).args(["--version", "--json"]),
    );
    let facts: Value = serde_json::from_str(facts.trim()).unwrap();
    assert_eq!(facts["commit_full"], new.as_str(), "{facts}");
    until(KBF_UNIT, || answers(STORAGE));
    let pid = systemctl("kbf_pid", &["show", "-p", "MainPID", "--value", KBF_UNIT]);
    let exe = fs::read_link(format!("/proc/{}/exe", pid.trim())).unwrap();
    assert_eq!(
        exe,
        Path::new(RELEASES).join(new).join("kbf"),
        "the restarted unit runs N+1's kbf"
    );

    let confirmed = ask(&probe, "confirm", &format!("confirm kbf={version}"));
    assert_eq!(confirmed["result"], "confirmed", "{confirmed}");
    let after = ask(&probe, "query_after", "query");
    assert_eq!(after["commit"], new.as_str(), "{after}");
    assert!(after["pending"].is_null(), "no trial is pending: {after}");
}
