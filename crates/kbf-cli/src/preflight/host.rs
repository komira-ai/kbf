//! The host the self-checks read: files, environment, limits. A trait so a
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
    /// A symbolic link (only [`Host::lstat`] reports one).
    Link,
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

/// What `lstat` says of a path: its [`Stat`] (a symbolic link is [`Kind::Link`]) and its group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lstat {
    /// The path itself, not what a link points at.
    pub stat: Stat,
    /// The owning group.
    pub gid: u32,
}

/// The host the checks read.
pub trait Host {
    /// `stat(path)`.
    ///
    /// # Errors
    /// The error `stat` returned.
    fn stat(&self, path: &Path) -> io::Result<Stat>;
    /// `lstat(path)`: like [`Host::stat`], but a symbolic link is itself, [`Kind::Link`], and
    /// the group is told.
    ///
    /// # Errors
    /// The error `lstat` returned.
    fn lstat(&self, path: &Path) -> io::Result<Lstat>;
    /// The bytes of the file at `path`.
    ///
    /// # Errors
    /// The error `read` returned.
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// An environment variable.
    fn var(&self, name: &str) -> Option<String>;
    /// The user this process runs as.
    fn euid(&self) -> u32;
    /// The group this process runs as.
    fn egid(&self) -> u32;
    /// The supplementary groups of this process.
    fn groups(&self) -> Vec<u32>;
    /// The soft open-file limit in force.
    fn nofile(&self) -> u64;
    /// The time now.
    fn now(&self) -> SystemTime;
}

/// The machine this process runs on.
#[derive(Clone, Copy, Debug)]
pub struct System;

impl Host for System {
    fn stat(&self, path: &Path) -> io::Result<Stat> {
        Ok(stat_of(&std::fs::metadata(path)?))
    }

    fn lstat(&self, path: &Path) -> io::Result<Lstat> {
        let m = std::fs::symlink_metadata(path)?;
        Ok(Lstat {
            stat: stat_of(&m),
            gid: m.gid(),
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

    fn egid(&self) -> u32 {
        rustix::process::getegid().as_raw()
    }

    fn groups(&self) -> Vec<u32> {
        rustix::process::getgroups()
            .unwrap_or_default()
            .iter()
            .map(|g| g.as_raw())
            .collect()
    }

    fn nofile(&self) -> u64 {
        rustix::process::getrlimit(rustix::process::Resource::Nofile)
            .current
            .unwrap_or(u64::MAX)
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// What `m` says of its file.
fn stat_of(m: &std::fs::Metadata) -> Stat {
    let kind = if m.is_file() {
        Kind::File
    } else if m.is_dir() {
        Kind::Dir
    } else if m.is_symlink() {
        Kind::Link
    } else {
        Kind::Other
    };
    Stat {
        kind,
        uid: m.uid(),
        mode: m.mode() & 0o7777,
    }
}
