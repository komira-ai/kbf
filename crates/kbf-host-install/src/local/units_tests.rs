//! Restarting a unit and waiting for it, with `systemctl` played by the test.

use std::io;
use std::time::Duration;

use super::units::restart_and_wait;
use crate::plan::Wait;
use crate::request::Unit;
use crate::runner::fake::{FakeRunner, exec, ok};

fn wait() -> Wait {
    Wait {
        timeout: Duration::from_millis(30),
        interval: Duration::from_millis(1),
    }
}

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
    restart_and_wait(&runner, &unit(), wait()).unwrap();
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
    let err = restart_and_wait(&runner, &unit(), wait()).unwrap_err();
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
    let err = restart_and_wait(&runner, &unit(), wait()).unwrap_err();
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
    let err = restart_and_wait(&runner, &unit(), wait()).unwrap_err();
    assert!(err.contains("was not active"), "{err}");
}
