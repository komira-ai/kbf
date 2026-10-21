//! The `launchctl` and `systemctl` command sequences of [`Services`], on a host that records
//! each command it is asked.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;

use kbf_host::testkit::{FakeExec, reply};

use super::roots::Roots;
use super::services::Services;
use super::units::Unit;
use crate::layout::Platform;

const JOB: &str = "com.komira.kbf-daemon.plist";

/// A host that records every command as one line; `loaded` says whether `print` finds the
/// job, and the first `lag` prints of a loaded job show no pid.
struct Played {
    exec: FakeExec,
    seen: Rc<RefCell<Vec<String>>>,
}

fn played(loaded: bool, lag: u32) -> Played {
    let loaded = Cell::new(loaded);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let lag = Cell::new(lag);
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new({
        let seen = seen.clone();
        move |cmd| {
            let line = format!("{} {}", cmd.program, cmd.args.join(" "));
            let print = line.starts_with("/bin/launchctl print");
            if line.starts_with("/bin/launchctl bootstrap") {
                loaded.set(true);
            } else if line.starts_with("/bin/launchctl bootout") {
                loaded.set(false);
            }
            seen.borrow_mut().push(line);
            if !print {
                return Ok(reply(0, "", ""));
            }
            if !loaded.get() {
                return Ok(reply(113, "", "Could not find service"));
            }
            if lag.get() > 0 {
                lag.set(lag.get() - 1);
                return Ok(reply(0, "state = waiting\n", ""));
            }
            Ok(reply(0, "\tpid = 812\n", ""))
        }
    });
    Played { exec, seen }
}

fn with<R>(
    played: &mut Played,
    platform: Platform,
    what: impl FnOnce(&mut Services<'_, FakeExec>) -> io::Result<R>,
) -> io::Result<R> {
    let roots = Roots::new(platform);
    what(&mut Services {
        exec: &mut played.exec,
        roots: &roots,
    })
}

fn job() -> Unit {
    Unit::new(JOB).expect("a launchd job")
}

const ENABLE: &str = "/bin/launchctl enable system/com.komira.kbf-daemon";
const BOOTSTRAP: &str =
    "/bin/launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-daemon.plist";
const PRINT: &str = "/bin/launchctl print system/com.komira.kbf-daemon";
const BOOTOUT: &str = "/bin/launchctl bootout system/com.komira.kbf-daemon";
const PAUSE: &str = "/bin/sleep 1";

/// Catches a launchd start that bootstraps a job still disabled, or that returns before the
/// job runs: `enable`, then `bootstrap`, then one look that finds the pid.
///
/// Mutants seen red: `enable` dropped; `enable` after `bootstrap`; the wait dropped.
#[test]
fn a_launchd_start_enables_bootstraps_and_looks_for_the_pid() {
    let mut host = played(false, 0);
    with(&mut host, Platform::MacOs, |s| s.start(&job())).expect("start");
    assert_eq!(*host.seen.borrow(), [PRINT, ENABLE, BOOTSTRAP, PRINT]);
}

/// Catches a start of a loaded job that bootstraps it as it is (a real launchd refuses): the
/// job is booted out first, then started as an unloaded one is.
///
/// Mutant seen red: the stop before the start dropped.
#[test]
fn a_launchd_start_boots_a_loaded_job_out_first() {
    let mut host = played(true, 0);
    with(&mut host, Platform::MacOs, |s| s.start(&job())).expect("start");
    assert_eq!(
        *host.seen.borrow(),
        [PRINT, BOOTOUT, ENABLE, BOOTSTRAP, PRINT]
    );
}

/// Catches a stop that asks `bootout` of a job launchd does not have (an error there), and
/// one that does not ask it of a loaded job.
///
/// Mutants seen red: `bootout` whether or not the job is loaded; the load check dropped.
#[test]
fn a_launchd_stop_boots_out_only_a_loaded_job() {
    let mut gone = played(false, 0);
    with(&mut gone, Platform::MacOs, |s| s.stop(&job())).expect("stop");
    assert_eq!(*gone.seen.borrow(), [PRINT]);

    let mut loaded = played(true, 0);
    with(&mut loaded, Platform::MacOs, |s| s.stop(&job())).expect("stop");
    assert_eq!(*loaded.seen.borrow(), [PRINT, BOOTOUT]);
}

/// Catches a wait that gives up early or does not pause: after three looks without a pid
/// the fourth finds it, with a one-second pause between looks.
///
/// Mutants seen red: the pause dropped; the loop ending after one look.
#[test]
fn the_start_waits_until_the_job_has_a_pid() {
    let mut host = played(false, 3);
    with(&mut host, Platform::MacOs, |s| s.start(&job())).expect("start");
    assert_eq!(
        *host.seen.borrow(),
        [
            PRINT, ENABLE, BOOTSTRAP, PRINT, PAUSE, PRINT, PAUSE, PRINT, PAUSE, PRINT
        ]
    );
}

/// Catches a start that waits forever or says nothing: a job that never runs fails after
/// thirty looks (twenty-nine pauses), as a timeout naming the unit.
///
/// Mutants seen red: the bound raised without the test knowing; the unit left out of the
/// message.
#[test]
fn a_job_that_never_runs_times_out_naming_the_unit() {
    let mut host = played(false, 1000);
    let error = with(&mut host, Platform::MacOs, |s| s.start(&job())).expect_err("times out");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains(JOB), "{error}");
    assert!(
        error.to_string().contains("loaded and not running"),
        "{error}"
    );
    let seen = host.seen.borrow();
    assert_eq!(seen.iter().filter(|l| *l == PAUSE).count(), 29);
    assert_eq!(seen.iter().filter(|l| *l == PRINT).count(), 31);
}

/// Catches the systemd arms picking up launchd's steps: start and stop are one command each,
/// with no look before or after.
#[test]
fn the_systemd_arms_are_one_command_each() {
    let unit = Unit::new("kbf-daemon.service").expect("a unit");
    let mut host = played(false, 0);
    with(&mut host, Platform::Linux, |s| {
        s.stop(&unit)?;
        s.start(&unit)
    })
    .expect("both");
    assert_eq!(
        *host.seen.borrow(),
        [
            "/usr/bin/systemctl stop -- kbf-daemon.service",
            "/usr/bin/systemctl start -- kbf-daemon.service"
        ]
    );
}
