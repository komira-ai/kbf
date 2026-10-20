//! Starting a program a test has just written.
//!
//! A test that copies a program (`fs::copy` of a built binary) and runs the copy can
//! find it busy: `exec` fails with `ETXTBSY` while any process holds a descriptor open
//! for writing on the file. The copy's own descriptor is closed when the copy returns,
//! but a process forked by *another thread of the test binary* while it was open (any
//! other test spawning a program, see [`crate::forked`]) holds a copy of it until that
//! child execs, a few milliseconds later. The file is whole and nothing is wrong with
//! it; the start has only to wait for the child to go.
//!
//! [`spawn_retrying`] starts the command again while it fails that way, a bounded
//! number of times, so a test can start a program it has just copied.

use std::io::{self, ErrorKind};
use std::process::{Child, Command};
use std::time::Duration;

/// How many times [`spawn_retrying`] tries to start the program.
pub const ATTEMPTS: u32 = 50;

/// How long [`spawn_retrying`] waits between tries: a second in all, far longer than a
/// fork takes to exec.
pub const PAUSE: Duration = Duration::from_millis(20);

/// `command.spawn()`, started again after [`PAUSE`] each time it fails because the
/// program is busy (`ETXTBSY`), at most [`ATTEMPTS`] times in all. Any other failure,
/// and the last busy one, is returned as it came.
///
/// # Errors
/// What `spawn` says.
pub fn spawn_retrying(command: &mut Command) -> io::Result<Child> {
    let mut started = None;
    let end = retry_busy(ATTEMPTS, PAUSE, &mut || {
        started = Some(command.spawn()?);
        Ok(())
    });
    end.map(|()| started.expect("a start that succeeded left its child"))
}

/// `start()`, called again after `pause` each time it fails because the program is busy
/// (`ETXTBSY`), at most `attempts` times in all (once, if `attempts` is less). Any other
/// failure, and the last busy one, is returned as it came.
///
/// # Errors
/// What `start` says.
pub fn retry_busy(
    attempts: u32,
    pause: Duration,
    start: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut left = attempts;
    loop {
        match start() {
            Err(error) if error.kind() == ErrorKind::ExecutableFileBusy && left > 1 => {
                left -= 1;
                std::thread::sleep(pause);
            }
            done => return done,
        }
    }
}
