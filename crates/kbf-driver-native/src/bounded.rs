//! A command's output, read within a deadline.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a killed child is waited for to be reaped, and its output read.
const REAP: Duration = Duration::from_secs(2);

/// The stdout of `command` (stdin closed, stderr dropped) if it exits with success within
/// `within`; `Err(true)` if it was still running then (it is killed and reaped, for up to
/// two seconds), `Err(false)` if it could not be started, failed, or exited with an error.
pub(crate) fn stdout_within(command: &mut Command, within: Duration) -> Result<Vec<u8>, bool> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| false)?;
    // Read as it is written: a listing is larger than a pipe holds. The reader reports
    // through a channel, so that no wait for it is unbounded.
    let stdout = child.stdout.take();
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_end(&mut text);
        }
        let _ = sent.send(text);
    });
    let give_up = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Ok(received.recv_timeout(REAP).unwrap_or_default());
            }
            Ok(Some(_)) | Err(_) => return Err(false),
            Ok(None) if Instant::now() < give_up => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let reaped = Instant::now() + REAP;
                while child.try_wait().is_ok_and(|s| s.is_none()) && Instant::now() < reaped {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a read that waits for a hung command (a lease end or a slot clean would
    /// hang with it): `sleep 30` is cut off near the deadline, reported as timed out,
    /// and not left running.
    #[test]
    fn a_command_that_hangs_is_cut_off_at_the_deadline() {
        let started = Instant::now();
        let got = stdout_within(Command::new("sleep").arg("30"), Duration::from_millis(300));
        assert_eq!(got, Err(true));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(5), "not cut off: {took:?}");
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
