//! What the verbs ask of the Mac, behind [`Host`]: run a program, read a path's type,
//! mode, link count and owner, the effective uid, and the file changes `apply` makes.
//! Files are read directly, under [`Host::root`]. [`RealHost`] is the Mac itself; the
//! tests' fake keeps each program's state in memory and serves the files from a
//! scratch directory.

use std::fs::{self, Metadata};
use std::io::{self, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
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

/// Everything the verbs ask of the Mac. `check` and `print` use only the reads (`root`,
/// `euid`, `run` of a read form, `lstat`); `apply`, `restart` and `revert` also change
/// it, through the programs it runs and the file methods. Every path is a path on the
/// Mac, joined to [`Host::root`].
pub trait Host {
    /// The directory that is the Mac's `/`: `/` on a node, a scratch directory in tests.
    fn root(&self) -> &str;
    /// The effective uid the verbs run as.
    fn euid(&self) -> u32;
    /// Runs the program at `program` (a path on the Mac) with `args`, in directory
    /// `dir` (a path on the Mac; `None` leaves it as it is), its standard input and
    /// error closed and its environment empty but for `LC_ALL=C` and `env`.
    ///
    /// # Errors
    /// The program could not be started or waited for.
    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        dir: Option<&str>,
        env: &[(&str, &str)],
    ) -> io::Result<Output>;
    /// `run_in` with no directory and no added environment.
    ///
    /// # Errors
    /// The program could not be started or waited for.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
        self.run_in(program, args, None, &[])
    }
    /// `lstat` of `path` on the Mac; `None` when there is nothing there.
    fn lstat(&self, path: &str) -> Option<Stat>;
    /// Writes `bytes` to the file at `path`, never through a link: it is created with
    /// `mode` (less the umask) if it is not there, and truncated, keeping its owner
    /// and mode, if it is.
    ///
    /// # Errors
    /// The file cannot be opened or written; a link at `path` is refused.
    fn write(&self, path: &str, bytes: &[u8], mode: u32) -> io::Result<()>;
    /// Makes the directory `path` and every missing directory above it.
    ///
    /// # Errors
    /// A directory cannot be made.
    fn mkdir(&self, path: &str) -> io::Result<()>;
    /// Sets the permission bits of `path`.
    ///
    /// # Errors
    /// There is nothing at `path`, or its mode cannot be set.
    fn chmod(&self, path: &str, mode: u32) -> io::Result<()>;
    /// Sets the owner of `path`; a link is changed itself, never what it names.
    ///
    /// # Errors
    /// There is nothing at `path`, or its owner cannot be set.
    fn chown(&self, path: &str, uid: u32, gid: u32) -> io::Result<()>;
    /// Renames `from` to `to`, replacing what is at `to`.
    ///
    /// # Errors
    /// The rename fails.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// Removes what is at `path`: a directory with everything in it, a link itself
    /// (never what it names). Nothing there is not an error.
    ///
    /// # Errors
    /// Something at `path` cannot be removed.
    fn remove(&self, path: &str) -> io::Result<()>;
    /// Waits `seconds` (between tries of a program that may not succeed at once).
    fn pause(&self, seconds: u64);
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

    fn on(&self, path: &str) -> String {
        on_root(&self.root, path)
    }
}

impl Host for RealHost {
    fn root(&self) -> &str {
        &self.root
    }

    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn run_in(
        &self,
        program: &str,
        args: &[&str],
        dir: Option<&str>,
        env: &[(&str, &str)],
    ) -> io::Result<Output> {
        let mut command = Command::new(self.on(program));
        command
            .args(args)
            .env_clear()
            .env("LC_ALL", "C")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = dir {
            command.current_dir(self.on(dir));
        }
        let out = command.output()?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }

    fn lstat(&self, path: &str) -> Option<Stat> {
        fs::symlink_metadata(self.on(path))
            .ok()
            .map(|m| Stat::of(&m))
    }

    fn write(&self, path: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
        use rustix::fs::{Mode, OFlags};
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::open(self.on(path), flags, Mode::from_raw_mode(mode as _))?;
        fs::File::from(fd).write_all(bytes)
    }

    fn mkdir(&self, path: &str) -> io::Result<()> {
        fs::create_dir_all(self.on(path))
    }

    fn chmod(&self, path: &str, mode: u32) -> io::Result<()> {
        fs::set_permissions(self.on(path), fs::Permissions::from_mode(mode))
    }

    fn chown(&self, path: &str, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::lchown(self.on(path), Some(uid), Some(gid))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.on(from), self.on(to))
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        let path = self.on(path);
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(m) if m.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
        }
    }

    fn pause(&self, seconds: u64) {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
    }
}

/// The calls the verbs share, over a [`Host`]. Every path is a path on the Mac.
#[derive(Clone, Copy)]
pub(crate) struct Mac<'a> {
    host: &'a dyn Host,
}

/// `result`, its error saying which path it is about.
fn at<T>(path: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))
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

    /// Whether `program args`, run in `dir` with `env` added, runs and exits 0.
    pub(crate) fn succeeds_in(
        &self,
        program: &str,
        args: &[&str],
        dir: Option<&str>,
        env: &[(&str, &str)],
    ) -> bool {
        self.host
            .run_in(program, args, dir, env)
            .is_ok_and(|o| o.success)
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

    /// Makes `path` (and the directories above it) a directory of `uid`:`gid` with
    /// `mode`.
    pub(crate) fn dir(&self, path: &str, uid: u32, gid: u32, mode: u32) -> io::Result<()> {
        at(path, self.host.mkdir(path))?;
        at(path, self.host.chown(path, uid, gid))?;
        at(path, self.host.chmod(path, mode))
    }

    /// Replaces the file at `path` with one of `uid`:`gid`, `mode`, holding `bytes`:
    /// written beside it as `<path>.kbf-new`, its owner and mode set, then renamed over
    /// it, so `path` is never a partial file or another owner's.
    pub(crate) fn replace(
        &self,
        path: &str,
        bytes: &[u8],
        uid: u32,
        gid: u32,
        mode: u32,
    ) -> io::Result<()> {
        let new = format!("{path}.kbf-new");
        at(&new, self.host.remove(&new))?;
        at(&new, self.host.write(&new, bytes, mode))?;
        at(&new, self.host.chown(&new, uid, gid))?;
        at(&new, self.host.chmod(&new, mode))?;
        at(path, self.host.rename(&new, path))
    }

    /// Empties the file at `path`, keeping its owner and mode; never through a link.
    pub(crate) fn truncate(&self, path: &str) -> io::Result<()> {
        at(path, self.host.write(path, b"", 0o600))
    }

    /// Makes `path` an empty file of `uid`:`gid` with `mode`, removing what was there.
    pub(crate) fn new_file(&self, path: &str, uid: u32, gid: u32, mode: u32) -> io::Result<()> {
        self.remove(path)?;
        at(path, self.host.write(path, b"", mode))?;
        at(path, self.host.chown(path, uid, gid))?;
        at(path, self.host.chmod(path, mode))
    }

    pub(crate) fn chmod(&self, path: &str, mode: u32) -> io::Result<()> {
        at(path, self.host.chmod(path, mode))
    }

    pub(crate) fn chown(&self, path: &str, uid: u32, gid: u32) -> io::Result<()> {
        at(path, self.host.chown(path, uid, gid))
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        at(from, self.host.rename(from, to))
    }

    pub(crate) fn remove(&self, path: &str) -> io::Result<()> {
        at(path, self.host.remove(path))
    }

    pub(crate) fn mkdir(&self, path: &str) -> io::Result<()> {
        at(path, self.host.mkdir(path))
    }

    pub(crate) fn pause(&self, seconds: u64) {
        self.host.pause(seconds);
    }
}
