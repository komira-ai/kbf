//! The releases a Mac keeps after the ssh path's apply: the ones it no longer keeps are removed
//! once the apply succeeded, and not otherwise.

use std::fs;
use std::time::{Duration, UNIX_EPOCH};

use super::fx::{Fx, INSTALL, NEW, OLD, Sim, binary, mkdir, write};
use crate::fixed_mark::mark;
use crate::mac::apply::run;
use crate::mac::kit;

const OLD_WELCOME: &str = "\x1b[32m INFO\x1b[0m kbf_daemon::daemon: welcomed server=old\n";
const BASE: u64 = 1_700_000_000;

fn commit_n(n: u32) -> String {
    format!("{n:02}{}", "d".repeat(38))
}

/// Install up to the launch of the apply job; the job's body is run by the test.
fn launched(fx: &Fx, sim: &Sim) {
    write(&fx.host.err_log, 0o644, OLD_WELCOME.as_bytes());
    let answer = fx.handle(sim, INSTALL);
    assert_eq!(answer.code, 0, "{}", answer.err);
}

fn age(fx: &Fx, commit: &str, at: u64) {
    fs::File::open(fx.host.release(commit))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(BASE + at))
        .unwrap();
}

/// Eight older releases than the one the apply installs, and OLD older still.
fn seed(fx: &Fx) {
    for n in 20..28 {
        let dir = fx.host.release(&commit_n(n));
        mkdir(&dir, 0o755);
        write(&dir.join("kbf"), 0o755, &binary("old"));
        age(fx, &commit_n(n), u64::from(n));
    }
    age(fx, OLD, 0);
}

fn dirs(fx: &Fx) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fx.host.releases())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A good apply leaves the release it installed, the one that ran before it (`previous`) and the
/// four newest others; the rest are removed, and the status says done.
///
/// Catches: an apply that never prunes, or prunes with another count.
#[test]
fn a_good_apply_removes_the_releases_beyond_the_newest_four() {
    let fx = Fx::new("mac-retain-ok");
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    assert_eq!(run(&fx.host, &sim), 0);
    assert_eq!(fx.status(), "done\n");
    let mut expected: Vec<String> = [27, 26, 25, 24].map(commit_n).to_vec();
    expected.extend([NEW.to_owned(), OLD.to_owned()]);
    expected.sort();
    assert_eq!(dirs(&fx), expected);
    let links = kit::links(&fx.host).unwrap();
    assert_eq!(links.current.as_deref(), Some(NEW));
    assert_eq!(links.previous.as_deref(), Some(OLD));
}

/// An apply that failed removes nothing: the daemon did not come up on the new release, or the
/// job found no plan.
///
/// Catches: a prune after a failed apply.
#[test]
fn a_failed_apply_removes_nothing() {
    let fx = Fx::new("mac-retain-failed");
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    sim.welcome.set(false);
    fs::remove_file(&fx.host.err_log).unwrap();
    write(&fx.host.err_log, 0o644, b"");
    let mut before = dirs(&fx);
    assert_eq!(run(&fx.host, &sim), 1);
    assert!(fx.status().starts_with("failed:"), "{}", fx.status());
    // The release was laid out before the daemon failed to come up on it; nothing else moved.
    before.push(NEW.to_owned());
    before.sort();
    assert_eq!(dirs(&fx), before);

    let planless = Fx::new("mac-retain-noplan");
    let sim = planless.sim();
    seed(&planless);
    let before = dirs(&planless);
    assert_eq!(run(&planless.host, &sim), 1);
    assert_eq!(dirs(&planless), before);
}

/// A prune that cannot be done (the retain file does not parse, and may hold a pin) removes
/// nothing and does not fail the apply, which is done; the audit log says why.
///
/// Catches: a failed prune failing an apply that is done; a failed prune that leaves no word of
/// why, so a host that never prunes again cannot be told from one that has nothing to remove.
#[test]
fn a_prune_that_fails_does_not_fail_the_apply() {
    let fx = Fx::new("mac-retain-prunefail");
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    write(&fx.host.work.join("retain.json"), 0o600, b"{ not json");
    let before = dirs(&fx);
    assert_eq!(run(&fx.host, &sim), 0);
    assert_eq!(fx.status(), "done\n");
    let mut expected = before.clone();
    expected.push(NEW.to_owned());
    expected.sort();
    assert_eq!(dirs(&fx), expected, "nothing was removed");
    let audit = fs::read_to_string(&fx.host.audit).unwrap();
    let line = audit
        .lines()
        .find(|l| l.contains("\"verb\":\"prune\""))
        .unwrap_or_else(|| panic!("no prune line: {audit}"));
    assert!(line.contains("\"outcome\":\"prune_failed\""), "{line}");
    assert!(line.contains("cannot be parsed"), "{line}");
}

/// A good apply's prune is in the audit log too, with what it kept and removed.
#[test]
fn a_good_apply_logs_its_prune() {
    let fx = Fx::new("mac-retain-prunelog");
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    assert_eq!(run(&fx.host, &sim), 0);
    let audit = fs::read_to_string(&fx.host.audit).unwrap();
    let line = audit
        .lines()
        .find(|l| l.contains("\"verb\":\"prune\""))
        .unwrap_or_else(|| panic!("no prune line: {audit}"));
    assert!(line.contains("\"outcome\":\"pruned\""), "{line}");
    assert!(line.contains("keeping: "), "{line}");
}

/// The pinned release and the coordinator's last good stay after an apply, however old.
#[test]
fn the_pinned_and_the_last_good_release_stay_after_an_apply() {
    let fx = Fx::new("mac-retain-pins");
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    write(
        &fx.host.work.join("retain.json"),
        0o600,
        format!(
            "{{\"pinned\":[\"{}\"],\"last_good\":\"{}\"}}",
            commit_n(20),
            commit_n(21)
        )
        .as_bytes(),
    );
    assert_eq!(run(&fx.host, &sim), 0);
    let mut expected: Vec<String> = [20, 21, 27, 26, 25, 24].map(commit_n).to_vec();
    expected.extend([NEW.to_owned(), OLD.to_owned()]);
    expected.sort();
    assert_eq!(dirs(&fx), expected);
}

/// An install and its apply never write the program's own path (the helper) or the agent's.
///
/// Catches: an apply that writes the helper's or the agent's fixed file.
#[test]
fn an_apply_never_writes_the_helper_or_the_agent() {
    let mut fx = Fx::new("mac-retain-guard");
    let libexec = fx.dir.join("usr/local/libexec/kbf");
    for dir in [
        "usr",
        "usr/local",
        "usr/local/libexec",
        "usr/local/libexec/kbf",
    ] {
        mkdir(&fx.dir.join(dir), 0o755);
    }
    let helper = libexec.join("kbf-host-install");
    let agent = libexec.join("kbf-updater");
    write(&helper, 0o755, b"the helper as it was");
    write(&agent, 0o755, b"the agent as it was");
    let marks = [mark(&helper), mark(&agent)];
    fx.host.self_exe = helper.clone();
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    assert_eq!(run(&fx.host, &sim), 0);
    assert_eq!([mark(&helper), mark(&agent)], marks, "neither is written");
    assert_eq!(fs::read_dir(&libexec).unwrap().count(), 2);
    assert!(dirs(&fx).len() < 10, "the prune ran");
}

/// An ssh apply that succeeds while the socket has a swap on trial (its marker is there, whole or
/// damaged) removes nothing: the trial may go back to a release only it names. The apply is done
/// all the same.
///
/// Catches: the apply's prune reading no trial, which removes releases while one is on trial.
#[test]
fn an_apply_while_the_socket_has_a_swap_on_trial_removes_nothing() {
    for (name, marker) in [
        (
            "mac-retain-trial",
            format!("{{\"commit\":\"{}\"}}", commit_n(27)),
        ),
        ("mac-retain-trial-bad", "{ not json".to_owned()),
    ] {
        let fx = Fx::new(name);
        let sim = fx.sim();
        seed(&fx);
        launched(&fx, &sim);
        let before = dirs(&fx);
        write(&fx.host.pending_marker(), 0o600, marker.as_bytes());
        assert_eq!(run(&fx.host, &sim), 0, "{name}");
        assert_eq!(fx.status(), "done\n", "{name}");
        let mut want = before.clone();
        want.push(NEW.to_owned());
        want.sort();
        want.dedup();
        assert_eq!(dirs(&fx), want, "{name}: nothing removed");
    }
}

/// An ssh apply that succeeds while another install or switch holds the host's install lock
/// removes nothing; the apply is done all the same.
///
/// Catches: the apply's prune going ahead beside an install or a switch of the socket.
#[test]
fn an_apply_while_the_install_lock_is_held_removes_nothing() {
    let fx = Fx::new("mac-retain-locked");
    let sim = fx.sim();
    seed(&fx);
    launched(&fx, &sim);
    let before = dirs(&fx);
    let held = crate::host::take_lock(&fx.host.lock_file()).unwrap();
    assert_eq!(run(&fx.host, &sim), 0);
    drop(held);
    let mut want = before;
    want.push(NEW.to_owned());
    want.sort();
    want.dedup();
    assert_eq!(dirs(&fx), want);
}
