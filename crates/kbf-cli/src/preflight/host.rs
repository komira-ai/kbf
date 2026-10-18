//! The host the self-checks read: files, environment, limits, one program. A trait so a
//! test plants a host instead of touching the real one.

use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::SystemTime;

/// What kind of file a path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// Anything else (a socket, a device, a fifo).
    Other,
}

/// What `stat` says of a path (symlinks followed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// The kind of file.
    pub kind: Kind,
    /// The owning user.
    pub uid: u32,
    /// The permission bits (and no file-type bits).
    pub mode: u32,
}

/// What a program printed and whether it succeeded.
#[derive(Clone, Debug)]
pub struct Output {
    /// Exit status 0.
    pub success: bool,
    /// Standard output, lossily decoded.
    pub stdout: String,
}

/// The host the checks read.
pub trait Host {
    /// `stat(path)`.
    ///
    /// # Errors
    /// The error `stat` returned.
    fn stat(&self, path: &Path) -> io::Result<Stat>;
    /// The bytes of the file at `path`.
    ///
    /// # Errors
    /// The error `read` returned.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// An environment variable.
    fn var(&self, name: &str) -> Option<String>;
    /// The user this process runs as.
    fn euid(&self) -> u32;
    /// The soft open-file limit in force.
    fn nofile(&self) -> u64;
    /// Runs `program` with `args` and collects its output.
    ///
    /// # Errors
    /// The program cannot be started.
    fn run(&self, program: &Path, args: &[&str]) -> io::Result<Output>;
    /// The time now.
    fn now(&self) -> SystemTime;
}

/// The machine this process runs on.
#[derive(Clone, Copy, Debug)]
pub struct System;

impl Host for System {
    fn stat(&self, path: &Path) -> io::Result<Stat> {
        let m = std::fs::metadata(path)?;
        let kind = if m.is_file() {
            Kind::File
        } else if m.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        Ok(Stat {
            kind,
            uid: m.uid(),
            mode: m.mode() & 0o7777,
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn nofile(&self) -> u64 {
        rustix::process::getrlimit(rustix::process::Resource::Nofile)
            .current
            .unwrap_or(u64::MAX)
    }

    fn run(&self, program: &Path, args: &[&str]) -> io::Result<Output> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
