//! Shared by the test files that run a binary they just copied.

use std::io;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// How long [`output_unbusy`] keeps starting a busy program.
pub const BUSY_WAIT: Duration = Duration::from_secs(5);
/// The pause between two tries of a busy program.
const BUSY_POLL: Duration = Duration::from_millis(2);

/// `command.output()`, started again while the program is busy until `wait` is up, then
/// the last error. A file this process wrote a moment ago is briefly open for writing in
/// a child that another test thread forked before it execs it (`O_CLOEXEC` closes the
/// descriptor at that exec, not before), and exec of a file open for writing is
/// `ETXTBSY`. Closing the file first does not close that window; waiting does.
pub fn output_unbusy(command: &mut Command, wait: Duration) -> io::Result<Output> {
    let give_up = Instant::now() + wait;
    loop {
        match command.output() {
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && Instant::now() < give_up => {
                std::thread::sleep(BUSY_POLL);
            }
            done => return done,
        }
    }
}
