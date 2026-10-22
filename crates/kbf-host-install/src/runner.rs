//! Running a program: an `execve` with an explicit argument vector and a cleared
//! environment, behind a trait so tests record the vectors instead of running them.
//!
//! No shell is involved anywhere: a [`Cmd`] is a program path and its arguments, and the
//! only environment a child sees is [`PATH`] and [`LANG`].

use std::io;
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
        todo!()
    }

    /// Stdout as text, invalid bytes replaced.
    pub fn stdout_text(&self) -> String {
        todo!()
    }

    /// Stderr as text, invalid bytes replaced.
    pub fn stderr_text(&self) -> String {
        todo!()
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
    fn run(&self, _cmd: &Cmd) -> io::Result<Exec> {
        let _ = (Command::new("x"), Stdio::null());
        todo!()
    }
}

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;
