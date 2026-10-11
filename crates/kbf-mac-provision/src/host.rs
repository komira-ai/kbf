//! What a check asks of the Mac, behind [`Host`]: run a program, read a path's type,
//! mode, link count and owner, and the effective uid. Files are read directly, under
//! [`Host::root`]. [`RealHost`] is the Mac itself; the tests' fake keeps each program's
//! state in memory and serves the files from a scratch directory.

use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// What a program printed on stdout, and whether it exited 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
}

/// A path's type, permission bits, link count and owner, as `lstat` reports them
/// (a link is not followed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// `ls`'s type letter: `-` a regular file, `d` a directory, `l` a symbolic link,
    /// `?` anything else.
    pub kind: char,
    /// The permission bits, set-id and sticky bits included.
    pub mode: u32,
    pub links: u64,
    pub uid: u32,
    pub gid: u32,
}

impl Stat {
    /// Reads the fields from `lstat`'s metadata.
    pub fn of(m: &Metadata) -> Self {
        let t = m.file_type();
        let kind = if t.is_dir() {
            'd'
        } else if t.is_symlink() {
            'l'
        } else if t.is_file() {
            '-'
        } else {
            '?'
        };
        Self {
            kind,
            mode: m.mode() & 0o7777,
            links: m.nlink(),
            uid: m.uid(),
            gid: m.gid(),
        }
    }

    /// The fields `ls -ldn` starts with: `-rw-r--r-- 0 0`.
    pub fn ls(&self) -> String {
        format!(
            "{} {} {}",
            mode_string(self.kind, self.mode),
            self.uid,
            self.gid
        )
    }
}

/// `ls`'s ten-character mode string: the type letter, then `rwx` for owner, group and
/// others, with `s`/`S` for set-uid and set-gid and `t`/`T` for the sticky bit.
pub fn mode_string(kind: char, mode: u32) -> String {
    let mut s = String::with_capacity(10);
    s.push(kind);
    for (shift, special, on) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        s.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        s.push(match (mode & special != 0, bits & 0o1 != 0) {
            (true, true) => on,
            (true, false) => on.to_ascii_uppercase(),
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// `path` on the Mac whose `/` is `root`.
pub fn on_root(root: &str, path: &str) -> String {
    format!("{}{path}", root.trim_end_matches('/'))
}

/// What the checks need from the Mac. No method changes it.
pub trait Host {
    /// The directory that is the Mac's `/`: `/` on a node, a scratch directory in tests.
    fn root(&self) -> &str;
    /// The effective uid the checks run as.
    fn euid(&self) -> u32;
    /// Runs the program at `program` (a path on the Mac) with `args`, its standard
    /// input and error closed and its environment empty but for `LC_ALL=C`.
    ///
    /// # Errors
    /// The program could not be started or waited for.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output>;
    /// `lstat` of `path` on the Mac; `None` when there is nothing there.
    fn lstat(&self, path: &str) -> Option<Stat>;
}

/// The Mac this runs on, with its `/` at `root` (`/` but in tests).
#[derive(Clone, Debug)]
pub struct RealHost {
    root: String,
}

impl RealHost {
    pub fn new(root: impl Into<String>) -> Self {
        Self { root: root.into() }
    }
}

impl Host for RealHost {
    fn root(&self) -> &str {
        &self.root
    }

    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        let out = Command::new(on_root(&self.root, program))
            .args(args)
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }

    fn lstat(&self, path: &str) -> Option<Stat> {
        fs::symlink_metadata(on_root(&self.root, path))
            .ok()
            .map(|m| Stat::of(&m))
    }
}

/// The reads the checks share, over a [`Host`]. Every path is a path on the Mac.
#[derive(Clone, Copy)]
pub(crate) struct Mac<'a> {
    host: &'a dyn Host,
}

impl<'a> Mac<'a> {
    pub(crate) fn new(host: &'a dyn Host) -> Self {
        Self { host }
    }

    pub(crate) fn euid(&self) -> u32 {
        self.host.euid()
    }

    /// Where `path` is on this machine.
    pub(crate) fn path(&self, path: &str) -> String {
        on_root(self.host.root(), path)
    }

    /// What `program args` prints on stdout, without its trailing newlines (as a shell's
    /// `$(...)` gives it), whatever its exit status; empty if it cannot run.
    pub(crate) fn out(&self, program: &str, args: &[&str]) -> String {
        self.host
            .run(program, args)
            .map(|o| o.stdout.trim_end_matches('\n').to_owned())
            .unwrap_or_default()
    }

    /// Whether `program args` runs and exits 0.
    pub(crate) fn succeeds(&self, program: &str, args: &[&str]) -> bool {
        self.host.run(program, args).is_ok_and(|o| o.success)
    }

    pub(crate) fn lstat(&self, path: &str) -> Option<Stat> {
        self.host.lstat(path)
    }

    /// `ls -ldn`'s fields for `path`, or `missing`.
    pub(crate) fn ls(&self, path: &str) -> String {
        self.lstat(path)
            .map_or_else(|| "missing".into(), |s| s.ls())
    }

    /// The file's bytes, following links; `None` if it cannot be read.
    pub(crate) fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.path(path)).ok()
    }

    /// Whether something is at `path`, following links (a dangling link is nothing).
    pub(crate) fn exists(&self, path: &str) -> bool {
        Path::new(&self.path(path)).exists()
    }

    /// Whether `path` is a directory, following links.
    pub(crate) fn is_dir(&self, path: &str) -> bool {
        Path::new(&self.path(path)).is_dir()
    }

    /// Whether `path` is a regular file, following links.
    pub(crate) fn is_file(&self, path: &str) -> bool {
        Path::new(&self.path(path)).is_file()
    }

    /// The names in directory `dir` that do not start with `.`, in byte order: what a
    /// shell's `dir/*` lists. Empty if `dir` cannot be read.
    pub(crate) fn list(&self, dir: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path(dir))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names
    }
}
