//! Running a program with a time limit: a `dscl` that hangs (the directory service not
//! answering) must fail the phase that started it, not hold the job until its limit.
//!
//! Pure `std` and `rustix`, so the timeout path is tested on every platform with a fake
//! program (`sleep`, `cat`); `mac.rs` starts every program it runs through [`output`].

use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// How long one program the spike starts may take. A `dscl` answers in tens of
/// milliseconds on an idle Mac and in seconds with a thousand users and `opendirectoryd`
/// busy; a minute is a hang.
pub const WITHIN: Duration = Duration::from_secs(60);

/// The error of a program that did not finish: killed at `limit`.
fn timed_out(program: &str, args: &[String], pid: u32, limit: Duration) -> String {
    format!("{program} {args:?} (pid {pid}): no answer within {limit:?}, killed")
}

/// Runs `cmd` (standard input is `stdin`, or closed), waits at most `limit` for it to
/// exit, and returns what it wrote. A program still running at `limit` is killed
/// (SIGKILL) and is an error; the call returns without waiting for it to be reaped, so a
/// process the kernel cannot end does not hold the caller either.
///
/// # Errors
/// The program could not be started or waited for, or did not exit within `limit`.
pub fn output(mut cmd: Command, stdin: Option<Vec<u8>>, limit: Duration) -> Result<Output, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{program}: {e}"))?;
    let pid = child.id();
    // Each pipe on a thread of its own: a program may answer before it has read all of its
    // input, and a full pipe must not stop it from exiting.
    if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), stdin) {
        thread::spawn(move || pipe.write_all(&bytes));
    }
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let (done, finished) = mpsc::channel();
    // Owns the child: it is reaped here whenever it ends, also after a kill.
    thread::spawn(move || {
        let _ = done.send(child.wait());
    });
    let status = match finished.recv_timeout(limit) {
        Ok(status) => status.map_err(|e| format!("{program} {args:?}: {e}"))?,
        Err(RecvTimeoutError::Timeout) => {
            kill(pid);
            return Err(timed_out(&program, &args, pid, limit));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(format!("{program} {args:?}: lost the process"));
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// SIGKILL to `pid`, which the waiter thread has not reaped (it reaps only after the
/// process ended, and the pid is not reused before that), so it is still the child.
fn kill(pid: u32) {
    if let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn command(program: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }

    /// Whether the process `pid` is gone (reaped), by `kill(pid, 0)`.
    fn gone(pid: u32) -> bool {
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).expect("pid")).expect("pid");
        rustix::process::test_kill_process(pid).is_err()
    }

    #[test]
    fn a_program_that_does_not_answer_is_an_error_at_the_limit_and_is_killed() {
        // `sleep 300` stands for a `dscl` that never answers.
        let started = Instant::now();
        let err = output(command("sleep", &["300"]), None, Duration::from_millis(300))
            .expect_err("must time out");
        assert!(err.contains("no answer within"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "returned at the limit, not when the program ended: {:?}",
            started.elapsed()
        );
        let pid: u32 = err
            .split("(pid ")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {err}"));
        // The kill is asynchronous with the return (the waiter thread reaps it).
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gone(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(pid), "pid {pid} still runs after the timeout");
    }

    #[test]
    fn a_program_that_ends_before_the_limit_gives_its_output_and_status() {
        let ok = output(
            command("cat", &[]),
            Some(b"hello".to_vec()),
            Duration::from_secs(30),
        )
        .expect("cat");
        assert!(ok.status.success());
        assert_eq!(ok.stdout, b"hello");
        let slow =
            output(command("sleep", &["0.2"]), None, Duration::from_secs(30)).expect("sleep");
        assert!(slow.status.success());
        let failed = output(command("false", &[]), None, Duration::from_secs(30)).expect("false");
        assert!(
            !failed.status.success(),
            "a non-zero exit is the caller's to read"
        );
    }

    #[test]
    fn input_and_output_larger_than_a_pipe_do_not_stop_the_program_from_ending() {
        let big = vec![b'x'; 4 << 20];
        let out = output(
            command("cat", &[]),
            Some(big.clone()),
            Duration::from_secs(60),
        )
        .expect("cat");
        assert_eq!(out.stdout.len(), big.len());
    }

    #[test]
    fn a_program_that_cannot_start_is_an_error_naming_it() {
        let err = output(
            command("/no/such/program", &[]),
            None,
            Duration::from_secs(5),
        )
        .expect_err("must fail");
        assert!(err.contains("/no/such/program"), "{err}");
    }
}
