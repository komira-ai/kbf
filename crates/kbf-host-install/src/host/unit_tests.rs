//! The body of the install unit: what runs inside `systemd-run`, under the install lock.
//!
//! The process sshd starts only starts the unit and relays its answer (`tests.rs`); everything
//! that touches the package, and the lock that covers it, is here.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;

use serde_json::{Value, json};

use super::tests::{Answer, Fx, INSTALL, channel, install_script, unreadable_cases, version};
use super::{handle, handle_unit};
use crate::locks;
use crate::plan;
use crate::runner::Runner;
use crate::runner::fake::{FakeRunner, exec, ok};

fn unit(fx: &Fx, runner: &dyn Runner, request: Option<&OsStr>) -> Answer {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = handle_unit(&fx.host, runner, request, &mut out, &mut err);
    Answer {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn install(fx: &Fx, runner: &dyn Runner) -> Answer {
    unit(fx, runner, Some(OsStr::new(INSTALL)))
}

/// The plan's four commands for an install over `before`, in order: the unit runs the apt
/// steps itself, with no unit around each, and every one of them runs while the install
/// lock is held. The answer is the install's line; no audit line is written (the process
/// that started the unit writes it); and the lock is let go at the end.
#[test]
fn the_unit_runs_both_steps_under_the_lock() {
    let fx = Fx::new("unit-install");
    let lock = fx.host.install_lock.clone();
    let seen = Rc::new(Cell::new(0));
    let counter = Rc::clone(&seen);
    let runner = install_script("ii  1.0-1\n", "ii  1.2.3-1\n").before_each(move |cmd| {
        assert!(locks::flock_held(&lock), "no lock while running {cmd:?}");
        counter.set(counter.get() + 1);
    });
    let answer = install(&fx, &runner);
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(
        answer.json(),
        json!({"result": "installed", "installed_version": "1.2.3-1", "unit_state": "finished"})
    );
    let v = version("1.2.3-1");
    assert_eq!(
        runner.calls(),
        vec![
            plan::installed_version(),
            plan::apt_update(&fx.channel_source()),
            plan::apt_install(&fx.channel_source(), &v),
            plan::installed_version(),
        ]
    );
    assert_eq!(seen.get(), 4, "the lock was looked at before every command");
    assert!(fx.audit_lines().is_empty(), "the unit does not audit");
    assert!(
        !locks::flock_held(&fx.host.install_lock),
        "released at the end"
    );
}

/// Catches the gap: between `apt-get update` and `apt-get install` (and before and after
/// each) `query` answers `in_progress`, though no unit is listed and dpkg holds nothing.
/// The lock is what says so, and the process doing the work is what holds it.
#[test]
fn no_moment_of_the_install_reads_as_idle() {
    let fx = Fx::new("unit-gap");
    let host = fx.host.clone();
    let asked = Rc::new(Cell::new(0));
    let counter = Rc::clone(&asked);
    let runner = install_script("ii  1.0-1\n", "ii  1.2.3-1\n").before_each(move |cmd| {
        // The state of the host as the updater would see it at this moment: a package
        // database, no kbf build, no unit listed.
        let probe = FakeRunner::new(vec![ok("ii  1.0-1\n"), exec(1, "", ""), ok("")]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = handle(&host, &probe, Some(OsStr::new("query")), &mut out, &mut err);
        let line: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(code, 0);
        assert_eq!(line["in_progress"], true, "idle before {cmd:?}");
        counter.set(counter.get() + 1);
    });
    assert_eq!(install(&fx, &runner).code, 0);
    assert_eq!(asked.get(), 4);

    // And once it is over it is idle again.
    locks::wait_lockable(&fx.host.install_lock);
    let probe = FakeRunner::new(vec![ok("ii  1.2.3-1\n"), exec(1, "", ""), ok("")]);
    let answer = fx.run(&probe, "query");
    assert_eq!(answer.json()["in_progress"], false);
}

/// A host with no `kbf` yet installs: `dpkg-query` exits 1 with nothing for an unknown
/// package, and a package that was removed but not purged (`rc`) is the same.
#[test]
fn installing_over_nothing() {
    for before in [
        exec(1, "", "dpkg-query: no packages found matching kbf"),
        ok("rc  1.0-1\n"),
        ok("un  \n"),
    ] {
        let fx = Fx::new("unit-fresh");
        let runner = FakeRunner::new(vec![before, ok(""), ok(""), ok("ii  1.2.3-1\n")]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 0, "{}", answer.err);
        assert_eq!(answer.json()["result"], "installed");
        assert_eq!(runner.calls().len(), 4);
    }
}

/// Equal is a no-op that reports installed: exit 0, only the read ran. Equal means equal
/// under dpkg's ordering, so `1.2.3-01` is the version `1.2.3-1` is.
#[test]
fn the_same_version_is_a_no_op() {
    for installed in ["1.2.3-1", "1.2.3-01", "0:1.2.3-1"] {
        let fx = Fx::new("unit-equal");
        let runner = FakeRunner::new(vec![ok(&format!("ii  {installed}\n"))]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 0, "{installed}: {}", answer.err);
        assert_eq!(
            answer.json(),
            json!({"result": "already_installed", "installed_version": installed, "unit_state": "not_run"})
        );
        assert_eq!(
            runner.calls(),
            vec![plan::installed_version()],
            "nothing else ran"
        );
    }
}

/// An older version is installed like any other: an older upstream, an older revision, a
/// higher epoch installed, a rebuild suffix, and a release candidate of the installed version.
/// Both apt steps run, the answer is `installed`, and the install step permits the downgrade
/// that `apt-get -y` would otherwise refuse.
#[test]
fn an_older_version_is_installed() {
    let cases = [
        ("1.2.4-1", "1.2.3-1"),
        ("1.2.3-2", "1.2.3-1"),
        ("1:0.1-1", "1.2.3-1"),
        ("2.0", "1.2.3-1"),
        ("1.2.3-1+b1", "1.2.3-1"),
        ("1.2.3-1", "1.2.3~rc1"),
    ];
    for (installed, target) in cases {
        let fx = Fx::new("unit-downgrade");
        let runner = FakeRunner::new(vec![
            ok(&format!("ii  {installed}\n")),
            ok(""),
            ok(""),
            ok(&format!("ii  {target}\n")),
        ]);
        let request = format!("install kbf={target} from candidate");
        let answer = unit(&fx, &runner, Some(OsStr::new(&request)));
        assert_eq!(answer.code, 0, "{installed} -> {target}: {}", answer.err);
        assert_eq!(
            answer.json(),
            json!({"result": "installed", "installed_version": target, "unit_state": "finished"})
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 4, "{installed} -> {target}: both steps ran");
        assert!(
            calls[2].args.iter().any(|a| a == "--allow-downgrades"),
            "{installed} -> {target}: the install step allows a downgrade"
        );
    }
}

/// A failing step stops the install: the update failing means no install ran, and the
/// detail (cleaned) is on stderr, not stdout.
#[test]
fn a_failed_update_stops_the_install() {
    for reply in [
        exec(100, "", "E: Failed to fetch\x1b[0m\n"),
        Err(std::io::Error::other("apt-get: not found")),
    ] {
        let fx = Fx::new("unit-update-fails");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), reply]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 3);
        assert_eq!(
            answer.json(),
            json!({"result": "failed", "reason": "update_failed"})
        );
        assert!(
            answer.err.contains("Failed to fetch") || answer.err.contains("not found"),
            "{}",
            answer.err
        );
        assert!(!answer.err.contains('\x1b'));
        assert_eq!(runner.calls().len(), 2, "the install step did not run");
        locks::wait_lockable(&fx.host.install_lock);
        assert!(!locks::flock_held(&fx.host.install_lock), "released");
    }
}

/// Some tools write their complaint to stdout: it is the detail when stderr is empty.
#[test]
fn the_detail_falls_back_to_stdout() {
    let fx = Fx::new("unit-detail");
    let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), exec(100, "E: on stdout\n", "")]);
    let answer = install(&fx, &runner);
    assert_eq!(answer.code, 3);
    assert!(answer.err.contains("E: on stdout"), "{}", answer.err);
}

/// The install step failing is its own code.
#[test]
fn a_failed_install_step() {
    for reply in [
        exec(100, "", "E: Unable to locate package kbf\n"),
        Err(std::io::Error::other("gone")),
    ] {
        let fx = Fx::new("unit-install-fails");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), ok(""), reply]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 3);
        assert_eq!(answer.json()["reason"], "install_failed");
        assert_eq!(runner.calls().len(), 3);
        assert!(!answer.err.is_empty());
    }
}

/// An install that ends well but leaves another version (or cannot be read back, or leaves
/// the package in error) is not reported installed.
#[test]
fn an_install_is_verified() {
    for after in [
        ok("ii  1.0-1\n"),
        ok("un  \n"),
        exec(2, "", "boom"),
        ok("ii  1.2.3-1+b1\n"),
        ok("iiR 1.2.3-1\n"),
    ] {
        let fx = Fx::new("unit-verify");
        let runner = FakeRunner::new(vec![ok("ii  1.0-1\n"), ok(""), ok(""), after]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 3);
        assert_eq!(answer.json()["reason"], "verify_failed");
    }
}

/// A read that cannot be trusted is a failure with exit 3 here too, before anything is
/// run, and the lock is let go.
#[test]
fn unreadable_state_fails_the_install() {
    for (what, reply, reason) in unreadable_cases() {
        let fx = Fx::new("unit-unreadable");
        let runner = FakeRunner::new(vec![reply()]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 3, "{what}");
        assert_eq!(
            answer.json(),
            json!({"result": "failed", "reason": reason}),
            "{what}"
        );
        assert_eq!(runner.calls().len(), 1, "{what}: nothing else ran");
        locks::wait_lockable(&fx.host.install_lock);
        assert!(!locks::flock_held(&fx.host.install_lock));
    }
}

/// A package dpkg flags `R` (reinstall required, or any flag in the third place) is not
/// cleanly installed. The install goes on to repair it: the same version is not a no-op
/// (apt reinstalls it), a newer one is installed over it; but it is no way round the
/// rule: an older target is installed as any other. And what the install leaves
/// must be clean, or it is not verified.
#[test]
fn a_package_in_error_is_installed_over() {
    for status in ["iiR", "iiH", "iiX"] {
        let fx = Fx::new("unit-flag-same");
        let runner = FakeRunner::new(vec![
            ok(&format!("{status} 1.2.3-1\n")),
            ok(""),
            ok(""),
            ok("ii  1.2.3-1\n"),
        ]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 0, "{status}: {}", answer.err);
        assert_eq!(answer.json()["result"], "installed", "{status}");
        assert_eq!(runner.calls().len(), 4, "{status}: apt ran");

        let fx = Fx::new("unit-flag-newer");
        let runner = FakeRunner::new(vec![
            ok(&format!("{status} 1.0-1\n")),
            ok(""),
            ok(""),
            ok("ii  1.2.3-1\n"),
        ]);
        assert_eq!(install(&fx, &runner).code, 0, "{status}");

        let fx = Fx::new("unit-flag-older");
        let runner = FakeRunner::new(vec![
            ok(&format!("{status} 1.2.4-1\n")),
            ok(""),
            ok(""),
            ok("ii  1.2.3-1\n"),
        ]);
        let answer = install(&fx, &runner);
        assert_eq!(answer.code, 0, "{status}: {}", answer.err);
        assert_eq!(runner.calls().len(), 4, "{status}: apt ran");
    }
}

/// Another install holds the lock: `busy`, nothing runs (not even the read), and the
/// holder's lock is not disturbed. Once the holder lets go the install goes through.
#[test]
fn a_busy_lock_runs_nothing() {
    let fx = Fx::new("unit-busy");
    let holder = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&fx.host.install_lock)
        .unwrap();
    holder.lock().unwrap();
    let runner = FakeRunner::new(vec![]);
    let answer = install(&fx, &runner);
    assert_eq!(answer.code, 3);
    assert_eq!(answer.json(), json!({"result": "failed", "reason": "busy"}));
    assert!(runner.calls().is_empty(), "nothing ran");
    assert!(
        locks::flock_held(&fx.host.install_lock),
        "still the holder's"
    );
    drop(holder);
    locks::wait_lockable(&fx.host.install_lock);
    let runner = install_script("ii  1.0-1\n", "ii  1.2.3-1\n");
    assert_eq!(install(&fx, &runner).code, 0);
    // And the lock is released at the end of that install.
    locks::wait_lockable(&fx.host.install_lock);
    let runner = FakeRunner::new(vec![ok("ii  1.2.3-1\n")]);
    assert_eq!(install(&fx, &runner).code, 0);
}

/// A lock file that cannot be opened fails the install closed, before anything runs; a
/// symbolic link in its place is not followed.
#[test]
fn an_unusable_lock_file_fails_closed() {
    let mut fx = Fx::new("unit-lock-missing");
    fx.host.install_lock = fx.dir.join("no-such-dir").join("lock");
    let runner = FakeRunner::new(vec![]);
    let answer = install(&fx, &runner);
    assert_eq!(answer.code, 3);
    assert_eq!(answer.json()["reason"], "lock_unavailable");
    assert!(runner.calls().is_empty());

    let target = fx.dir.join("elsewhere");
    fs::write(&target, "").unwrap();
    let link = fx.dir.join("lock-link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    fx.host.install_lock = link;
    assert_eq!(install(&fx, &runner).json()["reason"], "lock_unavailable");
    assert!(runner.calls().is_empty());
}

/// The body of the unit runs an install and nothing else: a query, a request outside the
/// grammar, none at all, and text that is not UTF-8 are refused, with exit 2 and no process.
#[test]
fn the_unit_runs_only_an_install() {
    let fx = Fx::new("unit-only-install");
    let bad: [Option<&OsStr>; 7] = [
        Some(OsStr::new("query")),
        Some(OsStr::new("install kbf=1;id from candidate")),
        Some(OsStr::new("install kbf=1 from candidate ")),
        Some(OsStr::new("")),
        Some(OsStr::new("install kbf=-1 from candidate")),
        Some(OsStr::from_bytes(b"install kbf=1 from candidate\xff")),
        None,
    ];
    for request in bad {
        let runner = FakeRunner::new(vec![]);
        let answer = unit(&fx, &runner, request);
        assert_eq!(answer.code, 2, "{request:?}");
        assert_eq!(
            answer.json(),
            json!({"result": "refused", "reason": "bad_command"}),
            "{request:?}"
        );
        assert!(runner.calls().is_empty(), "{request:?}");
    }
    locks::wait_lockable(&fx.host.install_lock);
    assert!(!locks::flock_held(&fx.host.install_lock));
}

/// The unit checks the allowlist and the source again, as the front does: what changed
/// between the two is refused with the front's codes, and nothing runs.
#[test]
fn the_unit_checks_the_allowlist_again() {
    let runner = FakeRunner::new(vec![]);
    let fx = Fx::new("unit-channel");
    let answer = unit(
        &fx,
        &runner,
        Some(OsStr::new("install kbf=1.2.3-1 from stable")),
    );
    assert_eq!(answer.json()["reason"], "channel_not_allowed");

    let fx = Fx::new("unit-config-gone");
    fs::remove_file(&fx.host.config).unwrap();
    assert_eq!(install(&fx, &runner).json()["reason"], "config_invalid");

    let fx = Fx::new("unit-unsigned");
    super::tests::write(
        &fx.source,
        0o644,
        "Types: deb\nURIs: https://apt.example.org/kbf\nSuites: candidate\n",
    );
    assert_eq!(install(&fx, &runner).json()["reason"], "channel_unsigned");

    let fx = Fx::new("unit-insecure");
    let key = fx.dir.join("key.gpg");
    super::tests::write(
        &fx.source,
        0o644,
        &format!(
            "deb [signed-by={} trusted=yes] https://apt.example.org/kbf candidate main\n",
            key.display()
        ),
    );
    let answer = install(&fx, &runner);
    assert_eq!(
        (answer.code, answer.json()["reason"].as_str()),
        (2, Some("channel_insecure"))
    );
    assert!(runner.calls().is_empty(), "no refusal started a process");
    let _ = channel("candidate");
}
