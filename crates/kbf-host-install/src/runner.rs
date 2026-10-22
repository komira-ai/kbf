//! Running a program: an `execve` with an explicit argument vector and a cleared
//! environment, behind a trait so tests record the vectors instead of running them.
//!
//! No shell is involved anywhere: a [`Cmd`] is a program path and its arguments, and the
//! only environment a child sees is [`PATH`] and [`LANG`].

use std::io;
use std::time::Duration;
use std::process::{Command, Stdio};

/// The only `PATH` a child sees.
pub const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// The only locale a child sees.
pub const LANG: &str = "C";

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
/// stderr captured.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        let output = Command::new(&cmd.program)
            .args(&cmd.args)
            .env_clear()
            .env("PATH", PATH)
            .env("LANG", LANG)
            .stdin(Stdio::null())
            .output()?;
        Ok(Exec {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
