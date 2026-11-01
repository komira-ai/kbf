//! Shared by the test files that run a binary they just copied.

use std::io;
use std::process::{Command, Output};
use std::time::Duration;

/// The pause between two tries of a busy program.
pub const BUSY_POLL: Duration = Duration::from_millis(2);

/// `command.output()`, started again while the program is busy. A file this process wrote
/// a moment ago is briefly open for writing in a child that another test thread forked
/// before it execs it (`O_CLOEXEC` closes the descriptor at that exec, not before), and
/// exec of a file open for writing is `ETXTBSY`. Closing the file first does not close
/// that window; waiting does. The retry has no limit of its own: a file that stays busy
/// is ended by the test runner's per-test limit.
pub fn output_unbusy(command: &mut Command) -> io::Result<Output> {
    output_unbusy_with(command, || std::thread::sleep(BUSY_POLL))
}

/// [`output_unbusy`] with the pause it is given, called between two tries.
pub fn output_unbusy_with(command: &mut Command, mut pause: impl FnMut()) -> io::Result<Output> {
    loop {
        match command.output() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy => pause(),
            done => return done,
        }
    }
}
