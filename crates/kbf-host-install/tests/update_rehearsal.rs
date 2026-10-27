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
//! workflow leaves its read-only token in `/var/tmp/kbf-update-rehearsal.token`. Every command's
//! output and the units' journal go to `target/update-rehearsal` (the workflow uploads it).

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

use serde_json::Value;

const REPO: &str = "komira-ai/kbf";
const TOKEN_FILE: &str = "/var/tmp/kbf-update-rehearsal.token";
const TO_FILE: &str = "/var/tmp/kbf-update-rehearsal.to";
/// The host's working directory: the downloads, and the copy of this test binary the updater's
/// account runs.
const WORK: &str = "/var/lib/kbf-update-rehearsal";
const SOCKET: &str = "/run/kbf-host-install/local.sock";
const RELEASES: &str = "/var/lib/kbf-installer/releases";
const HELPER: &str = "/usr/libexec/kbf/kbf-host-install";
const TRUST: &str = "/usr/share/kbf/trust";
const CHANNELS: &str = "/etc/kbf/host-install-releases.toml";
const UNIT_DIR: &str = "/etc/systemd/system";
const HELPER_UNIT: &str = "kbf-host-local.service";
/// The `kbf` unit the helper restarts.
const KBF_UNIT: &str = "kbf-rehearsal.service";
/// The `--restart-unit` of the packaged helper unit, replaced by [`KBF_UNIT`].
const PACKAGED_RESTART: &str = "--restart-unit kbf-daemon.service";
const CHANNEL: &str = "stable";
/// Where the kbf unit's storage node listens.
const STORAGE: &str = "127.0.0.1:8983";
const UPDATER: &str = "kbf-updater";
/// The environment variable that carries a request to [`speak_to_the_helper`].
const REQUEST_ENV: &str = "KBF_REHEARSAL_REQUEST";
/// How long one request may take: an install downloads and verifies a release.
const REQUEST_LIMIT: Duration = Duration::from_secs(900);
/// How many times a unit may restart before it is ready: more means it fails at start, and the
/// rehearsal stops instead of waiting for the job's limit.
const MAX_RESTARTS: u32 = 3;

const KBF_UNIT_TEXT: &str = "\
[Unit]
Description=kbf storage node of the update rehearsal, run from the current release

[Service]
DynamicUser=yes
StateDirectory=kbf-rehearsal
WorkingDirectory=/var/lib/kbf-rehearsal
ExecStart=/var/lib/kbf-installer/releases/current/kbf --mode=storage --hot-store=file:///var/lib/kbf-rehearsal/hot --cold-store=file:///var/lib/kbf-rehearsal/cold --storage-listen=127.0.0.1:8983
Restart=on-failure
RestartSec=2
";

/// Where the raw outputs go.
fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/update-rehearsal")
}

fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Writes a command's output under `name`.
fn capture(name: &str, out: &Output) {
    let dir = out_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.stdout")), &out.stdout).unwrap();
    fs::write(dir.join(format!("{name}.stderr")), &out.stderr).unwrap();
    fs::write(
        dir.join(format!("{name}.code")),
        format!("{:?}\n", out.status.code()),
    )
    .unwrap();
}

/// Runs `cmd`, keeps its output as `name`, and requires it to succeed.
fn run(name: &str, cmd: &mut Command) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{name}: did not start: {e}"));
    capture(name, &out);
    assert!(
        out.status.success(),
        "{name} failed ({:?}): {}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn gh(token: &str) -> Command {
    let mut cmd = Command::new("gh");
    cmd.env("GH_TOKEN", token);
    cmd
}

/// The published releases, newest first: not drafts, not pre-releases.
fn release_tags(token: &str) -> Vec<String> {
    let text = run(
        "releases",
        gh(token).args([
            "api",
            &format!("repos/{REPO}/releases?per_page=30"),
            "--jq",
            ".[] | select(.draft | not) | select(.prerelease | not) | .tag_name",
        ]),
    );
    text.lines().map(str::to_owned).collect()
}

/// The commit a release's `SHA256SUMS` names for the Linux x86_64 tarball of `kbf`.
fn commit_of(sums: &str) -> String {
    let found: Vec<&str> = sums
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter_map(|name| name.strip_prefix("kbf-"))
        .filter_map(|rest| rest.strip_suffix("-linux-x86_64.tar.gz"))
        .filter(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect();
    assert_eq!(found.len(), 1, "one kbf linux-x86_64 tarball in SHA256SUMS");
    found[0].to_owned()
}

/// One release's files, downloaded into `dir` and checked against its `SHA256SUMS`.
fn download(token: &str, tag: &str, dir: &Path, names: &[String]) {
    fs::create_dir_all(dir).unwrap();
    let mut cmd = gh(token);
    cmd.args(["release", "download", tag, "-R", REPO, "-D"])
        .arg(dir)
        .args(["--clobber", "-p", "SHA256SUMS"]);
    for name in names {
        cmd.args(["-p", name]);
    }
    run(&format!("download_{tag}"), &mut cmd);
    if names.is_empty() {
        return;
    }
    let checked = run(
        &format!("sha256_{tag}"),
        Command::new("sha256sum")
            .args(["--check", "--strict", "--ignore-missing", "SHA256SUMS"])
            .current_dir(dir),
    );
    for name in names {
        assert!(
            checked.lines().any(|l| l == format!("{name}: OK")),
            "{tag}: {name} is not checked OK: {checked}"
        );
    }
}

/// The helper's answer line, which [`speak_to_the_helper`] prints, parsed.
fn answer(text: &str) -> Value {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("answer: "))
        .unwrap_or_else(|| panic!("no answer line: {text}"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

/// The units' journal and the helper's state, written when the rehearsal ends, whichever way.
struct Journal;

impl Drop for Journal {
    fn drop(&mut self) {
        let mut cmd = Command::new("journalctl");
        cmd.args(["--no-pager", "-u", HELPER_UNIT, "-u", KBF_UNIT]);
        if let Ok(out) = cmd.output() {
            capture("journal", &out);
            if std::thread::panicking() {
                eprintln!("{}", String::from_utf8_lossy(&out.stdout));
            }
        }
    }
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

/// The uid and gid of an account, from `/etc/passwd`.
fn ids_of(account: &str) -> (u32, u32) {
    let passwd = fs::read_to_string("/etc/passwd").unwrap();
    let fields: Vec<&str> = passwd
        .lines()
        .find(|l| l.starts_with(&format!("{account}:")))
        .unwrap_or_else(|| panic!("no account {account}"))
        .split(':')
        .collect();
    (fields[2].parse().unwrap(), fields[3].parse().unwrap())
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

fn systemctl(name: &str, args: &[&str]) -> String {
    run(name, Command::new("systemctl").args(args))
}

/// How many times `unit` has been restarted by systemd.
fn restarts(unit: &str) -> u32 {
    systemctl("restarts", &["show", "-p", "NRestarts", "--value", unit])
        .trim()
        .parse()
        .unwrap()
}

/// Waits until `ready` holds, and stops the rehearsal when `unit` restarts more than
/// [`MAX_RESTARTS`] times first: it fails at start, and would never be ready.
fn until(unit: &str, mut ready: impl FnMut() -> bool) {
    while !ready() {
        let n = restarts(unit);
        assert!(
            n <= MAX_RESTARTS,
            "{unit} restarted {n} times before it was ready"
        );
        sleep(Duration::from_secs(1));
    }
}

/// Whether the storage node of the kbf unit accepts a connection on its port.
fn storage_answers() -> bool {
    std::net::TcpStream::connect(STORAGE).is_ok()
}

fn install_file(text: &str, path: &str, mode: u32) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
#[ignore = "needs Linux, systemd, root and the network: cargo xtask sudo-test"]
fn the_helper_updates_a_hand_laid_host_from_the_last_release_to_the_next() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run through `cargo xtask sudo-test`"
    );
    let token = fs::read_to_string(TOKEN_FILE)
        .unwrap_or_else(|e| panic!("{TOKEN_FILE}: {e}"))
        .trim()
        .to_owned();
    let tags = release_tags(&token);
    let to = match fs::read_to_string(TO_FILE) {
        Ok(text) => text.trim().to_owned(),
        Err(_) => tags.first().expect("a published release").clone(),
    };
    let from = tags
        .iter()
        .skip_while(|t| **t != to)
        .nth(1)
        .unwrap_or_else(|| panic!("no published release before {to} in {tags:?}"))
        .clone();
    println!("rehearsing the update from {from} to {to}");

    let work = Path::new(WORK);
    fs::create_dir_all(work).unwrap();
    fs::set_permissions(work, fs::Permissions::from_mode(0o755)).unwrap();

    // The commits, from each release's SHA256SUMS; N+1's version, from its version facts.
    let (old_dir, new_dir) = (work.join(&from), work.join(&to));
    download(&token, &from, &old_dir, &[]);
    let old = commit_of(&fs::read_to_string(old_dir.join("SHA256SUMS")).unwrap());
    download(&token, &to, &new_dir, &[]);
    let new = commit_of(&fs::read_to_string(new_dir.join("SHA256SUMS")).unwrap());
    assert_ne!(old, new, "{from} and {to} are the same commit");
    let facts_name = format!("kbf-{new}-linux-x86_64.version.json");
    download(&token, &to, &new_dir, std::slice::from_ref(&facts_name));
    let facts: Value =
        serde_json::from_str(&fs::read_to_string(new_dir.join(&facts_name)).unwrap()).unwrap();
    let version = format!(
        "{}+g{}",
        facts["version"].as_str().expect("a version"),
        &new[..12]
    );

    // Release N's files: kbf, the helper, kbf-deploy and the package (for its trust files and
    // accounts).
    let names = ["kbf", "kbf-host-install", "kbf-deploy"]
        .map(|c| format!("{c}-{old}-linux-x86_64.tar.gz"))
        .to_vec();
    let deb = format!("kbf-{old}-linux-x86_64.deb");
    let mut all = names.clone();
    all.push(deb.clone());
    download(&token, &from, &old_dir, &all);
    for (name, into) in names.iter().zip(["release", "helper", "deploy"]) {
        let dir = old_dir.join(into);
        fs::create_dir_all(&dir).unwrap();
        run(
            &format!("untar_{into}"),
            Command::new("tar")
                .arg("-xzf")
                .arg(old_dir.join(name))
                .arg("-C")
                .arg(&dir),
        );
    }
    let unpacked = old_dir.join("deb");
    run(
        "dpkg_deb",
        Command::new("dpkg-deb")
            .arg("-x")
            .arg(old_dir.join(&deb))
            .arg(&unpacked),
    );

    let _journal = Journal;
    // The trust files and accounts of N's package, then the updater's account of this checkout.
    run(
        "trust_dir",
        Command::new("install").args(["-d", "-o", "root", "-g", "root", "-m", "0755", TRUST]),
    );
    for entry in fs::read_dir(unpacked.join(TRUST.trim_start_matches('/'))).unwrap() {
        let path = entry.unwrap().path();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        run(
            "trust_file",
            Command::new("install")
                .args(["-o", "root", "-g", "root", "-m", &format!("{mode:o}")])
                .arg(&path)
                .arg(Path::new(TRUST).join(path.file_name().unwrap())),
        );
    }
    let sysusers = unpacked.join("usr/lib/sysusers.d");
    if sysusers.is_dir() {
        for entry in fs::read_dir(&sysusers).unwrap() {
            run(
                "sysusers_package",
                Command::new("systemd-sysusers").arg(entry.unwrap().path()),
            );
        }
    }
    let host_agent = checkout().join("packaging/host-agent/linux");
    run(
        "sysusers_updater",
        Command::new("systemd-sysusers").arg(host_agent.join("kbf-updater.conf")),
    );

    // Release N laid out by its own kbf-deploy, and its helper.
    run(
        "host_install",
        Command::new(old_dir.join("deploy/kbf-deploy"))
            .args(["host", "install", "--commit", &old, "--from"])
            .arg(old_dir.join("release"))
            .arg("--apply"),
    );
    run(
        "helper_binary",
        Command::new("install")
            .args(["-D", "-o", "root", "-g", "root", "-m", "0755"])
            .arg(old_dir.join("helper/kbf-host-install"))
            .arg(HELPER),
    );
    fs::create_dir_all("/etc/kbf").unwrap();
    // The farm's nodes name their channel the same way, with no token.
    install_file(
        &format!("[channels.{CHANNEL}]\nsource = \"github\"\n"),
        CHANNELS,
        0o644,
    );

    // The units: the kbf unit, and the helper's unit of this checkout restarting it.
    install_file(KBF_UNIT_TEXT, &format!("{UNIT_DIR}/{KBF_UNIT}"), 0o644);
    let packaged = fs::read_to_string(host_agent.join(HELPER_UNIT)).unwrap();
    assert_eq!(packaged.matches(PACKAGED_RESTART).count(), 1, "{packaged}");
    let helper_unit = packaged.replace(PACKAGED_RESTART, &format!("--restart-unit {KBF_UNIT}"));
    install_file(&helper_unit, &format!("{UNIT_DIR}/{HELPER_UNIT}"), 0o644);
    systemctl("daemon_reload", &["daemon-reload"]);
    systemctl("start_kbf", &["start", KBF_UNIT]);
    systemctl("start_helper", &["start", HELPER_UNIT]);
    until(HELPER_UNIT, || Path::new(SOCKET).exists());
    until(KBF_UNIT, storage_answers);

    // The updater's account runs a copy of this binary it can reach.
    let probe = work.join("probe");
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
    assert_eq!(staged["result"], "staged", "{staged}");
    assert_eq!(staged["commit"], new.as_str(), "{staged}");

    let switched = ask(&probe, "switch", &format!("switch kbf={version}"));
    assert_eq!(switched["result"], "switched", "{switched}");

    // The unit runs N+1's binary.
    let current = fs::read_link(Path::new(RELEASES).join("current")).unwrap();
    assert_eq!(current, Path::new(&new), "the current link");
    let facts = run(
        "current_version",
        Command::new(Path::new(RELEASES).join("current/kbf")).args(["--version", "--json"]),
    );
    let facts: Value = serde_json::from_str(facts.trim()).unwrap();
    assert_eq!(facts["commit_full"], new.as_str(), "{facts}");
    until(KBF_UNIT, storage_answers);
    let pid = systemctl("kbf_pid", &["show", "-p", "MainPID", "--value", KBF_UNIT]);
    let exe = fs::read_link(format!("/proc/{}/exe", pid.trim())).unwrap();
    assert_eq!(
        exe,
        Path::new(RELEASES).join(&new).join("kbf"),
        "the restarted unit runs N+1's kbf"
    );

    let confirmed = ask(&probe, "confirm", &format!("confirm kbf={version}"));
    assert_eq!(confirmed["result"], "confirmed", "{confirmed}");
    let after = ask(&probe, "query_after", "query");
    assert_eq!(after["commit"], new.as_str(), "{after}");
    assert!(after["pending"].is_null(), "no trial is pending: {after}");
}
