//! A command's output, read within a deadline.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::pace::{Pace, Wall};

/// How long the output of a child that has exited is waited for.
const REAP: Duration = Duration::from_secs(2);

/// The stdout of `command` (stdin closed, stderr dropped) if it exits with success within
/// `within`; `Err(true)` if it was still running then (it is killed and reaped on another thread), `Err(false)` if it could not be started, failed, or exited with an error.
pub(crate) fn stdout_within(command: &mut Command, within: Duration) -> Result<Vec<u8>, bool> {
    stdout_paced(&Wall::new(), command, within)
}

/// [`stdout_within`] with the deadline read on `pace`.
fn stdout_paced(pace: &dyn Pace, command: &mut Command, within: Duration) -> Result<Vec<u8>, bool> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| false)?;
    // Read as it is written: a listing is larger than a pipe holds. The reader reports
    // through a channel, so that no wait for it is unbounded.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        let _ = sent.send(text);
    });
    let give_up = pace.now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Ok(received.recv_timeout(REAP).unwrap_or_default());
            }
            Ok(Some(_)) | Err(_) => return Err(false),
            Ok(None) if pace.now() < give_up => pace.pause(Duration::from_millis(10)),
            Ok(None) => {
                // Reaped off this thread: a process stuck in the kernel is waited for
                // there, not here.
                std::thread::spawn(move || {
                    let _ = child.kill();
                    let _ = child.wait();
                });
                return Err(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pace::Manual;

    /// Catches a read that waits for a hung command (a lease end or a slot clean would
    /// hang with it): `sleep 30` is cut off at the deadline, reported as timed out. The
    /// deadline is read on the test's clock, so the call is cut off at exactly the
    /// 300 ms, not before and not later, and the test does not wait for it; a `sleep 30`
    /// cannot exit in the instant that clock takes to run it out.
    #[test]
    fn a_command_that_hangs_is_cut_off_at_the_deadline() {
        let pace = Manual::default();
        let got = stdout_paced(
            &pace,
            Command::new("sleep").arg("30"),
            Duration::from_millis(300),
        );
        assert_eq!(got, Err(true));
        assert_eq!(pace.now(), Duration::from_millis(300));
    }

    /// Catches a command that cannot be started taken for a timeout (a caller would
    /// log a hang that was a missing program).
    #[test]
    fn a_command_that_cannot_start_is_not_a_timeout() {
        let got = stdout_within(
            &mut Command::new("/nonexistent/kbf-program"),
            Duration::from_secs(1),
        );
        assert_eq!(got, Err(false));
    }

    /// Catches output lost or a failure taken for success: a command that finishes in
    /// time gives its stdout, one that exits with an error gives none.
    #[test]
    fn a_command_that_finishes_gives_its_output_and_a_failure_none() {
        let got = stdout_within(Command::new("echo").arg("hi"), Duration::from_secs(10));
        assert_eq!(got, Ok(b"hi\n".to_vec()));
        let failed = stdout_within(&mut Command::new("false"), Duration::from_secs(10));
        assert_eq!(failed, Err(false));
    }
}
