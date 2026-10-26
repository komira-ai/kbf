//! Restarting a unit and waiting for it, with `systemctl` played by the test.

use std::io;
use std::time::Duration;

use super::units::restart_and_wait;
use crate::pace::fake::Fake;
use crate::plan::Wait;
use crate::request::Unit;
use crate::runner::fake::{FakeRunner, exec, ok};

fn wait() -> Wait {
    Wait {
        timeout: Duration::from_millis(30),
        interval: Duration::from_millis(1),
    }
}

const INTERVAL: Duration = Duration::from_millis(1);

fn unit() -> Unit {
    Unit::service("kbf-daemon.service").unwrap()
}

fn lines(runner: &FakeRunner) -> Vec<String> {
    runner
        .calls()
        .iter()
        .map(|c| format!("{} {}", c.program, c.args.join(" ")))
        .collect()
}

/// A unit is restarted and then looked at until it says it is active: a unit that is starting
/// (not active yet) is looked at again.
#[test]
fn a_restarted_unit_is_looked_at_until_it_is_active() {
    let runner = FakeRunner::new(vec![
        ok(""),
        exec(3, "activating", ""),
        exec(3, "", ""),
        ok("active"),
    ]);
    let clock = Fake::new();
    restart_and_wait(&runner, &unit(), wait(), &clock.pace()).unwrap();
    assert_eq!(
        clock.sleeps(),
        [INTERVAL, INTERVAL],
        "one pause after each look that found it not active, none after the last"
    );
    assert_eq!(
        lines(&runner),
        [
            "/usr/bin/systemctl restart -- kbf-daemon.service",
            "/usr/bin/systemctl is-active -- kbf-daemon.service",
            "/usr/bin/systemctl is-active -- kbf-daemon.service",
            "/usr/bin/systemctl is-active -- kbf-daemon.service",
        ]
    );
}

/// A restart that fails, that cannot be run, or a unit that is not active in time is an error that
/// names the unit; a look that cannot be made counts as not active.
#[test]
fn a_restart_that_fails_or_a_unit_that_never_comes_up_is_an_error() {
    let runner = FakeRunner::new(vec![exec(5, "", "Unit not found.")]);
    let err = restart_and_wait(&runner, &unit(), wait(), &Fake::new().pace()).unwrap_err();
    assert!(
        err.contains("kbf-daemon.service") && err.contains("Unit not found"),
        "{err}"
    );
    assert_eq!(
        runner.calls().len(),
        1,
        "no look at a unit that did not restart"
    );

    let runner = FakeRunner::new(vec![Err(io::Error::other("no systemctl"))]);
    let err = restart_and_wait(&runner, &unit(), wait(), &Fake::new().pace()).unwrap_err();
    assert!(err.contains("no systemctl"), "{err}");

    let mut script = vec![ok("")];
    script.extend((0..200).map(|i| {
        if i % 2 == 0 {
            exec(3, "inactive", "")
        } else {
            Err(io::Error::other("cannot ask"))
        }
    }));
    let runner = FakeRunner::new(script);
    let clock = Fake::new();
    let err = restart_and_wait(&runner, &unit(), wait(), &clock.pace()).unwrap_err();
    assert!(err.contains("was not active"), "{err}");
    // A look at every millisecond from the restart to the 30th, which is the last: the look
    // that finds the time up is the one that gives up.
    assert_eq!(clock.sleeps().len(), 30);
    assert_eq!(runner.calls().len(), 1 + 31, "the restart and 31 looks");
}

/// A unit that comes up only after more looks than the wait has time for is an error, and
/// one that comes up at the last look the wait allows is not: the wait is its timeout in
/// looks, however long a look takes.
#[test]
fn a_unit_that_is_active_at_the_last_look_is_up_and_one_look_later_is_not() {
    let script = |inactive: usize| {
        let mut script = vec![ok("")];
        script.extend((0..inactive).map(|_| exec(3, "activating", "")));
        script.push(ok("active"));
        script.extend((0..50).map(|_| ok("active")));
        FakeRunner::new(script)
    };
    // Looks are made at 0, 1, ... 30 ms.
    assert!(restart_and_wait(&script(30), &unit(), wait(), &Fake::new().pace()).is_ok());
    let err = restart_and_wait(&script(31), &unit(), wait(), &Fake::new().pace()).unwrap_err();
    assert!(err.contains("was not active"), "{err}");
}
