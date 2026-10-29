//! `restart frontend=<name>`: the commands it runs, the lock it holds, its answers and its
//! audit line.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};

use super::tests::{Fx, INSTALL, write};
use super::{Host, handle, handle_unit};
use crate::locks;
use crate::plan::{self, Wait};
use crate::request::Unit;
use crate::runner::Exec;
use crate::runner::Runner;
use crate::runner::fake::{FakeRunner, exec, ok};

/// What the plain frontend and the instance `a1` are called as a name and as a unit.
const NAMES: [(&str, &str); 2] = [
    ("-", "kbf-frontend.service"),
    ("a1", "kbf-frontend@a1.service"),
];

const INSTALLED_LINE: &str =
    r#"{"result":"installed","installed_version":"1.2.3-1","unit_state":"finished"}"#;

/// What the unit says when it never goes active.
const TIMEOUT_DETAIL: &str = "kbf-frontend@a1.service was not active";

fn unit(name: &str) -> Unit {
    Unit::frontend(name).unwrap()
}

/// The answer of a request that finds the install lock held: exit status 3, reason `busy`.
fn busy() -> (u8, String) {
    (3, "busy".to_owned())
}

/// The status and the reason of the line that the process sshd starts answers to `text`.
fn front_answer(host: &Host, runner: &dyn Runner, text: &str) -> (u8, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = handle(host, runner, Some(OsStr::new(text)), &mut out, &mut err);
    (code, reason_of(&out))
}

/// The status and the reason of the line that the body of the install unit answers to `text`.
fn body_answer(host: &Host, runner: &dyn Runner, text: &str) -> (u8, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = handle_unit(host, runner, Some(OsStr::new(text)), &mut out, &mut err);
    (code, reason_of(&out))
}

fn reason_of(out: &[u8]) -> String {
    let line: Value = serde_json::from_slice(out).unwrap();
    line["reason"].as_str().unwrap().to_owned()
}

/// Asserts that a restart runs `systemctl restart` and then `systemctl is-active`, and no
/// other command, for the plain unit and for an instance; that the install lock is held
/// before each of them and let go at the end; that the answer is the `restarted` line naming
/// the unit; and that the audit line names the verb `restart`, the unit and the outcome.
#[test]
fn a_restart_runs_two_commands_under_the_lock() {
    for (name, unit_name) in NAMES {
        let fx = Fx::new("restart-ok");
        let lock = fx.host.install_lock.clone();
        let seen = Rc::new(Cell::new(0));
        let counter = Rc::clone(&seen);
        let runner = FakeRunner::new(vec![ok(""), ok("active\n")]).before_each(move |cmd| {
            assert!(locks::flock_held(&lock), "no lock while running {cmd:?}");
            counter.set(counter.get() + 1);
        });
        let answer = fx.run(&runner, &format!("restart frontend={name}"));
        assert_eq!(answer.code, 0, "{}", answer.err);
        assert_eq!(
            answer.json(),
            json!({"result": "restarted", "unit": unit_name})
        );
        let u = unit(name);
        let (restart, active) = (plan::restart_unit(&u), plan::unit_active(&u));
        assert_eq!(runner.calls(), vec![restart, active]);
        assert_eq!(seen.get(), 2, "the lock was looked at before each command");
        locks::wait_lockable(&fx.host.install_lock);
        assert!(
            !locks::flock_held(&fx.host.install_lock),
            "released at the end"
        );
        let audit = fx.audit_lines();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0]["verb"], "restart");
        assert_eq!(audit[0]["unit"], unit_name);
        assert_eq!(audit[0]["outcome"], "restarted");
        assert_eq!(audit[0]["reason"], Value::Null);
    }
}

/// Asserts that a unit that is still `activating` is asked about again until it is active,
/// and that a look at it that cannot be made counts as not active yet; that the wait pauses
/// one interval after each look that found it not active and not after the one that found
/// it active (three pauses for four looks).
#[test]
fn a_restart_waits_until_the_unit_is_active() {
    let fx = Fx::new("restart-wait");
    let runner = FakeRunner::new(vec![
        ok(""),
        exec(3, "activating\n", ""),
        Err(io::Error::other("systemctl: gone")),
        exec(3, "activating\n", ""),
        ok("active\n"),
    ]);
    let answer = fx.run(&runner, "restart frontend=a1");
    assert_eq!(answer.code, 0, "{}", answer.err);
    assert_eq!(answer.json()["result"], "restarted");
    let calls = runner.calls();
    assert_eq!(calls.len(), 5);
    for call in &calls[1..] {
        assert_eq!(*call, plan::unit_active(&unit("a1")));
    }
    assert_eq!(
        fx.clock.sleeps(),
        [fx.host.restart_wait.interval; 3],
        "one pause after each look that found it not active"
    );
}

/// Runs `restart frontend=<name>` against a `systemctl restart` that answers `reply`, and
/// asserts that the answer is `restart_failed` with exit status 3, that nothing asks about
/// the unit afterwards, that stderr holds each of `detail`, and that the audit line names the
/// failure and the unit.
fn restart_fails(name: &str, reply: io::Result<Exec>, detail: &[&str]) {
    let fx = Fx::new("restart-fails");
    let runner = FakeRunner::new(vec![reply]);
    let answer = fx.run(&runner, &format!("restart frontend={name}"));
    assert_eq!(answer.code, 3, "{}", answer.err);
    assert_eq!(
        answer.json(),
        json!({"result": "failed", "reason": "restart_failed"})
    );
    for word in detail {
        assert!(answer.err.contains(word), "{word}: {}", answer.err);
    }
    assert_eq!(runner.calls().len(), 1, "no look at the unit followed");
    let audit = fx.audit_lines();
    assert_eq!(audit[0]["verb"], "restart");
    assert_eq!(audit[0]["outcome"], "failed");
    assert_eq!(audit[0]["reason"], "restart_failed");
    assert_eq!(audit[0]["unit"], unit(name).as_str());
}

/// Asserts that a `systemctl restart` that exits with a status, writes only to stdout, cannot
/// be started, runs past its limit, or is ended by a signal is a `restart_failed` whose
/// detail carries the exit status and what the command wrote.
#[test]
fn a_failed_restart_carries_the_exit() {
    restart_fails(
        "a1",
        exec(5, "", "Unit kbf-frontend@a1.service not found.\n"),
        &["exit status Some(5)", "not found"],
    );
    restart_fails(
        "-",
        exec(1, "Job for kbf-frontend.service failed.\n", ""),
        &["exit status Some(1)", "Job for"],
    );
    restart_fails(
        "a1",
        Err(io::Error::other("no such file")),
        &["/usr/bin/systemctl", "no such file"],
    );
    restart_fails(
        "a1",
        Err(io::Error::other("ran past its limit")),
        &["/usr/bin/systemctl", "ran past its limit"],
    );
    let signalled = Ok(Exec {
        code: None,
        signal: None,
        stdout: vec![],
        stderr: b"killed".to_vec(),
    });
    restart_fails("a1", signalled, &["exit status None", "killed"]);
}

/// Asserts that a unit that is not active when the wait is over is a `restart_timeout` with
/// exit status 3 whose detail names the unit and the seconds waited, and that the audit line
/// names the failure: after exactly one look and no pause when the wait is zero; and, for a
/// wait of two seconds with a look every half second, after five looks (at 0, 0.5, 1, 1.5
/// and 2 seconds of the made-up clock, the last of which finds the time up) and four pauses
/// of an interval each.
#[test]
fn a_unit_that_never_goes_active_times_out() {
    let mut fx = Fx::new("restart-timeout");
    fx.host.restart_wait = Wait {
        timeout: Duration::ZERO,
        interval: Duration::ZERO,
    };
    let runner = FakeRunner::new(vec![ok(""), exec(3, "activating\n", "")]);
    let answer = fx.run(&runner, "restart frontend=a1");
    assert_eq!(answer.code, 3, "{}", answer.err);
    assert_eq!(
        answer.json(),
        json!({"result": "failed", "reason": "restart_timeout"})
    );
    assert!(answer.err.contains(TIMEOUT_DETAIL), "{}", answer.err);
    assert_eq!(runner.calls().len(), 2, "one restart and one look");
    assert!(fx.clock.sleeps().is_empty(), "no pause before giving up");
    assert_eq!(fx.audit_lines()[0]["reason"], "restart_timeout");

    let interval = Duration::from_millis(500);
    fx.host.restart_wait = Wait {
        timeout: Duration::from_secs(2),
        interval,
    };
    let mut replies = vec![ok("")];
    replies.extend((0..10).map(|_| exec(3, "activating\n", "")));
    let runner = FakeRunner::new(replies);
    let answer = fx.run(&runner, "restart frontend=-");
    assert_eq!(answer.code, 3, "{}", answer.err);
    assert_eq!(answer.json()["reason"], "restart_timeout");
    assert!(
        answer
            .err
            .contains("kbf-frontend.service was not active 2 seconds after the restart"),
        "{}",
        answer.err
    );
    assert_eq!(runner.calls().len(), 1 + 5, "the restart and five looks");
    assert_eq!(fx.clock.sleeps(), [interval; 4]);
}

/// Asserts that a restart that finds the install lock held is `busy`, runs nothing and is
/// audited; that it goes through once the lock is let go; and that a lock file that cannot be
/// opened fails the restart closed before anything runs.
#[test]
fn a_held_lock_refuses_a_restart() {
    let fx = Fx::new("restart-busy");
    let holder = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&fx.host.install_lock)
        .unwrap();
    holder.lock().unwrap();
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, "restart frontend=a1");
    assert_eq!(answer.code, 3);
    assert_eq!(answer.json(), json!({"result": "failed", "reason": "busy"}));
    assert!(runner.calls().is_empty(), "nothing ran");
    assert_eq!(fx.audit_lines().len(), 1, "the attempt is audited");
    drop(holder);
    locks::wait_lockable(&fx.host.install_lock);
    let runner = FakeRunner::new(vec![ok(""), ok("active\n")]);
    assert_eq!(fx.run(&runner, "restart frontend=a1").code, 0);

    let mut fx = Fx::new("restart-lock-missing");
    fx.host.install_lock = fx.dir.join("no-such-dir").join("lock");
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, "restart frontend=a1");
    assert_eq!(answer.json()["reason"], "lock_unavailable");
    assert!(runner.calls().is_empty(), "nothing ran");
}

/// Asserts that while a restart runs, both the install request and the body of the install
/// unit are answered `busy` and run nothing: a restart and an install never overlap.
#[test]
fn an_install_cannot_overlap_a_restart() {
    let fx = Fx::new("restart-overlap");
    let host = fx.host.clone();
    let tried = Rc::new(Cell::new(0));
    let counter = Rc::clone(&tried);
    let runner = FakeRunner::new(vec![ok(""), ok("active\n")]).before_each(move |_| {
        let idle = FakeRunner::new(vec![]);
        assert_eq!(front_answer(&host, &idle, INSTALL), busy());
        assert_eq!(body_answer(&host, &idle, INSTALL), busy());
        assert!(idle.calls().is_empty(), "the install ran nothing");
        counter.set(counter.get() + 1);
    });
    assert_eq!(fx.run(&runner, "restart frontend=a1").code, 0);
    assert_eq!(tried.get(), 2, "installs tried during both commands");
}

/// Asserts that a restart is not started when the audit log cannot be opened: exit status 3,
/// `audit_unavailable`, a warning on stderr and no command run.
#[test]
fn no_restart_without_an_audit_log() {
    let fx = Fx::new("restart-no-audit");
    write(&fx.host.audit, 0o644, "");
    let runner = FakeRunner::new(vec![]);
    let answer = fx.run(&runner, "restart frontend=a1");
    assert_eq!(answer.code, 3);
    assert_eq!(
        answer.json(),
        json!({"result": "failed", "reason": "audit_unavailable"})
    );
    assert!(answer.err.contains("audit"), "{}", answer.err);
    assert!(runner.calls().is_empty(), "nothing ran");
}

/// Asserts that a restart request outside the grammar is refused with exit status 2 and
/// `bad_command`, and runs nothing.
#[test]
fn a_refused_restart_runs_nothing() {
    let fx = Fx::new("restart-refused");
    let runner = FakeRunner::new(vec![]);
    for command in [
        "restart frontend=",
        "restart frontend=A",
        "restart frontend=a;b",
        "restart frontend=../x",
        "restart unit=kbf-daemon.service",
        "restart frontend=a1 ",
        "restart frontend=a1 now",
    ] {
        let answer = fx.run(&runner, command);
        assert_eq!(answer.code, 2, "{command}");
        assert_eq!(
            answer.json(),
            json!({"result": "refused", "reason": "bad_command"}),
            "{command}"
        );
    }
    assert!(runner.calls().is_empty(), "nothing ran");
}

/// Asserts that the body of the install unit refuses a restart request, with exit status 2
/// and no command run: only an install is run under the unit.
#[test]
fn the_install_unit_runs_no_restart() {
    let fx = Fx::new("restart-in-unit");
    let runner = FakeRunner::new(vec![]);
    let answer = body_answer(&fx.host, &runner, "restart frontend=a1");
    assert_eq!(answer, (2, "bad_command".to_owned()));
    assert!(runner.calls().is_empty(), "nothing ran");
}

/// Asserts that a restart does not read the channel allowlist: it is carried out on a host
/// that has none.
#[test]
fn a_restart_does_not_read_the_allowlist() {
    let fx = Fx::new("restart-no-config");
    fs::remove_file(&fx.host.config).unwrap();
    let runner = FakeRunner::new(vec![ok(""), ok("active\n")]);
    assert_eq!(fx.run(&runner, "restart frontend=-").code, 0);
}

/// Asserts that only the audit line of a restart has a `unit`: the lines of a query and of an
/// install have none.
#[test]
fn only_a_restart_line_names_a_unit() {
    let fx = Fx::new("restart-audit");
    let runner = FakeRunner::new(vec![
        ok("un  \n"),
        ok(""),
        ok(INSTALLED_LINE),
        ok(""),
        ok("active\n"),
    ]);
    fx.run(&runner, "query");
    fx.run(&runner, INSTALL);
    fx.run(&runner, "restart frontend=a1");
    let audit = fx.audit_lines();
    assert_eq!(audit.len(), 3);
    assert!(audit[0].get("unit").is_none());
    assert!(audit[1].get("unit").is_none());
    assert_eq!(audit[2]["unit"], "kbf-frontend@a1.service");
}
