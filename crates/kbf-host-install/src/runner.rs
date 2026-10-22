//! Running a program: an `execve` with an explicit argument vector and a cleared
//! environment, behind a trait so tests record the vectors instead of running them.
//!
//! No shell is involved anywhere: a [`Cmd`] is a program path and its arguments, and the
//! only environment a child sees is [`PATH`], [`LANG`] and [`DEBIAN_FRONTEND`].

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// The only `PATH` a child sees.
pub const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The only locale a child sees.
pub const LANG: &str = "C";

/// The only front end `debconf` and `dpkg` see: they never ask a question.
pub const DEBIAN_FRONTEND: &str = "noninteractive";

/// A program (an absolute path) and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// How long it may run before it is killed; none to wait for it however long it takes.
    pub timeout: Option<Duration>,
}

/// How a child ended and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// The exit status, or none when a signal ended it.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Exec {
    /// Whether it exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Stdout as text, invalid bytes replaced.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as text, invalid bytes replaced.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Something that runs a [`Cmd`] and waits for it.
pub trait Runner {
    /// Run `cmd` to its end.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, cmd: &Cmd) -> io::Result<Exec>;
}

/// The real runner: `std::process::Command` with `env_clear`, stdin closed, stdout and
/// stderr captured, and the command killed when it outlives its limit.
pub struct SystemRunner;

/// How often a command with a limit is looked at.
const POLL: Duration = Duration::from_millis(10);

/// How long to wait for the output of a command that was killed (a grandchild may hold
/// the pipe open); what has not arrived by then is dropped.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// A thread that reads `stream` to its end and sends what it read.
fn drain<R: Read + Send + 'static>(stream: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        // A read that fails part way keeps what came before it.
        stream.into_iter().for_each(|mut stream| {
            let _ = stream.read_to_end(&mut bytes);
        });
        let _ = send.send(bytes);
    });
    receive
}

/// Wait for `child` for at most `limit`; none when it is still running then.
fn wait_within(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        thread::sleep(POLL.min(left));
    }
}

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .env_clear()
            .env("PATH", PATH)
            .env("LANG", LANG)
            .env("DEBIAN_FRONTEND", DEBIAN_FRONTEND)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        // Both streams are read while the child runs, or it blocks on a full pipe.
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = match cmd.timeout {
            None => Some(child.wait()?),
            Some(limit) => wait_within(&mut child, limit)?,
        };
        let Some(status) = status else {
            // Past its limit: kill it and reap it. A kill that fails means it just ended.
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{} ran past its limit and was killed", cmd.program),
            ));
        };
        let collect =
            |bytes: mpsc::Receiver<Vec<u8>>| bytes.recv_timeout(DRAIN_GRACE).unwrap_or_default();
        Ok(Exec {
            code: status.code(),
            stdout: collect(stdout),
            stderr: collect(stderr),
        })
    }
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
