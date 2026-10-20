//! `kbf-mac-session node-admin`: the verbs a Mac node's administrator runs as root
//! through `sudo`, with the guards every call goes through first.
//!
//! The mode is entered with `node-admin` as the binary's first argument; nothing
//! after it is a flag of the helper. The verbs so far are `phase0 status|apply|revert`
//! (see [`phase0`]) and `role-user status|create` (see [`role_user`]); every other verb
//! is refused with the usage text.
//!
//! Every call, in this order:
//! 1. refuses unless the effective uid is 0;
//! 2. refuses a `SUDO_UID` that is not a number, and a `SUDO_UID` without a
//!    `SUDO_USER` or the other way round (sudo sets both);
//! 3. checks the tree it works in: `/`, `/usr`, `/usr/local`, `/usr/local/kbf` and its
//!    `sbin`, `bin`, `etc`, `log` and `state`, and the running binary and every
//!    directory above it, must each be root's, writable by root alone (no group or
//!    other write bit, no access control list), and not a symbolic link;
//! 4. removes temporary files (`.incoming.*`) older than [`STALE`] from `state`, and from
//!    `/private/var/at` when that folder passes `role-user`'s check of it, which a call
//!    cut short (a crash, a power cut) left behind;
//! 5. appends one line to `log/admin.log` (see [`log_line`]), even for a verb it then
//!    refuses; the log is renamed `admin.log.1` once it is over [`ADMIN_LOG_MAX`];
//! 6. parses the verb: a closed list, each taking no further argument;
//! 7. for a verb that changes the Mac, takes an exclusive `flock` on
//!    `state/node-admin.lock`, waiting at most [`LOCK_WAIT`] seconds for another call
//!    to finish. The kernel releases it when the process ends, however it ends.
//!
//! Programs run by absolute path with an empty environment (on macOS,
//! [`run_command`](crate::autologin::run_command)): nothing of the caller's
//! environment that sudo keeps (`HOME`, `TZ`, `TMPDIR`, ...) reaches them, and the
//! helper itself reads only `SUDO_UID` and `SUDO_USER`.
//!
//! A refusal prints `REFUSED: <why>` and exits 2; a step that failed prints
//! `FAILED: <why>` and exits 1.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustix::fs::FlockOperation;

use crate::autologin::CommandOutput;

pub mod phase0;
pub mod role_user;

/// The first argument that selects this mode.
pub const MODE: &str = "node-admin";

/// How long a changing verb waits for another call's lock, in [`AdminHost::pause`]s
/// (one second each on a Mac), before it refuses.
pub const LOCK_WAIT: usize = 60;

/// `admin.log` is renamed `admin.log.1` (replacing it) once it is larger than this.
pub const ADMIN_LOG_MAX: u64 = 1024 * 1024;

/// How many bytes of each argument `admin.log` keeps.
pub const LOG_ARG_MAX: usize = 512;

/// How many arguments `admin.log` keeps per call.
pub const LOG_ARGS_MAX: usize = 16;

/// A temporary file this old belongs to no running call.
pub const STALE: Duration = Duration::from_secs(600);

/// The prefix of the helper's temporary files.
const INCOMING: &str = ".incoming.";

/// Group and other write bits.
const GROUP_OTHER_WRITE: u32 = 0o022;

/// What the OS does for the mode. The macOS implementation is in `crate::macos`.
pub trait AdminHost {
    /// The directory every path is under: `/` on a Mac, a scratch directory in tests.
    fn root(&self) -> &Path;
    /// The effective uid.
    fn euid(&self) -> u32;
    /// The running binary's path.
    ///
    /// # Errors
    /// The OS did not say.
    fn current_exe(&self) -> io::Result<PathBuf>;
    /// `path`'s owner, mode and type, not following a link ([`lstat`] on a real
    /// host); `None` when nothing is there.
    ///
    /// # Errors
    /// `path` could not be looked at.
    fn lstat(&self, path: &Path) -> io::Result<Option<Meta>>;
    /// Whether `path` has an access control list.
    ///
    /// # Errors
    /// The list could not be read.
    fn has_acl(&self, path: &Path) -> io::Result<bool>;
    /// Runs `program` (an absolute path) with `args` and an empty environment.
    ///
    /// # Errors
    /// The program could not be started or waited for.
    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
    /// Gives `path` (never followed when it is a link) to `uid` and `gid`.
    ///
    /// # Errors
    /// The owner could not be changed.
    fn chown(&self, path: &Path, uid: u32, gid: u32) -> io::Result<()>;
    /// The wall clock.
    fn now(&self) -> SystemTime;
    /// Waits a second between two tries of the lock.
    fn pause(&self);
}

/// A file's type, as [`Meta`] gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    File,
    Link,
    Other,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Directory => "a directory",
            Self::File => "a regular file",
            Self::Link => "a symbolic link",
            Self::Other => "neither a file nor a directory",
        })
    }
}

/// What the guards look at of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub uid: u32,
    /// The permission bits (`0o7777`).
    pub mode: u32,
    pub kind: Kind,
    pub size: u64,
    pub modified: SystemTime,
}

/// `path`'s [`Meta`], not following a link; `None` when nothing is there.
///
/// # Errors
/// `path` could not be looked at.
pub fn lstat(path: &Path) -> io::Result<Option<Meta>> {
    let meta = match fs::symlink_metadata(path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    let kind = meta.file_type();
    let kind = if kind.is_symlink() {
        Kind::Link
    } else if kind.is_dir() {
        Kind::Directory
    } else if kind.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Ok(Some(Meta {
        uid: meta.uid(),
        mode: meta.mode() & 0o7777,
        kind,
        size: meta.size(),
        modified: meta.modified()?,
    }))
}

/// Why a call stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Refused before changing anything it was asked to change: exit 2.
    Refused(String),
    /// A step failed: exit 1.
    Failed(String),
    /// The verb was not understood: the usage text, exit 2.
    Usage,
}

/// Where the mode works, under [`AdminHost::root`].
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub(crate) root: PathBuf,
    /// `/usr/local/kbf`.
    base: PathBuf,
    log: PathBuf,
    admin_log: PathBuf,
    pub(crate) state: PathBuf,
    lock: PathBuf,
}

impl Paths {
    /// The paths under `root`.
    #[must_use]
    pub(crate) fn under(root: &Path) -> Self {
        let base = root.join("usr/local/kbf");
        let log = base.join("log");
        let state = base.join("state");
        Self {
            root: root.to_owned(),
            admin_log: log.join("admin.log"),
            lock: state.join("node-admin.lock"),
            base,
            log,
            state,
        }
    }

    /// Every directory [`check_tree`] requires, from the root down.
    fn tree(&self) -> Vec<PathBuf> {
        let mut tree = vec![
            self.root.clone(),
            self.root.join("usr"),
            self.root.join("usr/local"),
            self.base.clone(),
        ];
        tree.extend(["sbin", "bin", "etc"].map(|dir| self.base.join(dir)));
        tree.extend([self.log.clone(), self.state.clone()]);
        tree
    }
}

/// Who ran sudo, as sudo's `SUDO_USER` and `SUDO_UID` say.
#[derive(Clone, Debug, Default)]
pub struct Sudo {
    pub user: Option<OsString>,
    pub uid: Option<OsString>,
}

/// The caller [`Sudo`] names: root when root ran the helper without sudo.
struct Caller {
    user: String,
    uid: u32,
}

impl Sudo {
    fn caller(&self) -> Result<Caller, Stop> {
        match (&self.user, &self.uid) {
            (None, None) => Ok(Caller {
                user: "root".to_owned(),
                uid: 0,
            }),
            (Some(user), Some(uid)) => match uid.to_str().map(str::parse) {
                Some(Ok(uid)) => Ok(Caller {
                    user: user.to_string_lossy().into_owned(),
                    uid,
                }),
                _ => Err(Stop::Refused(format!("SUDO_UID {uid:?} is not a number"))),
            },
            _ => Err(Stop::Refused(
                "sudo sets SUDO_USER and SUDO_UID together; only one of them is set".to_owned(),
            )),
        }
    }
}

/// The verbs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Phase0Status,
    Phase0Apply,
    Phase0Revert,
    RoleUserStatus,
    RoleUserCreate,
}

impl Verb {
    /// The verb `args` name; `Usage` for anything else, a refusal for a verb given an
    /// argument it does not take.
    ///
    /// # Errors
    /// `args` name no verb, or give one more than it takes.
    pub fn parse(args: &[OsString]) -> Result<Self, Stop> {
        let words: Vec<&[u8]> = args.iter().map(|arg| arg.as_bytes()).collect();
        let (verb, rest) = match words.as_slice() {
            [b"phase0", b"status", rest @ ..] => (Self::Phase0Status, rest),
            [b"phase0", b"apply", rest @ ..] => (Self::Phase0Apply, rest),
            [b"phase0", b"revert", rest @ ..] => (Self::Phase0Revert, rest),
            [b"role-user", b"status", rest @ ..] => (Self::RoleUserStatus, rest),
            [b"role-user", b"create", rest @ ..] => (Self::RoleUserCreate, rest),
            _ => return Err(Stop::Usage),
        };
        if rest.is_empty() {
            Ok(verb)
        } else {
            Err(Stop::Refused(format!(
                "'{verb}' takes no arguments (got {})",
                rest.len()
            )))
        }
    }

    /// Whether the verb changes the Mac (and so holds the lock).
    #[must_use]
    pub fn changes(self) -> bool {
        !matches!(self, Self::Phase0Status | Self::RoleUserStatus)
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Phase0Status => "phase0 status",
            Self::Phase0Apply => "phase0 apply",
            Self::Phase0Revert => "phase0 revert",
            Self::RoleUserStatus => "role-user status",
            Self::RoleUserCreate => "role-user create",
        })
    }
}

/// The usage text.
pub const USAGE: &str = "usage: sudo kbf-mac-session node-admin <verb>\n  phase0 status|apply|revert\n  role-user status|create";

/// Runs one call: the guards, then the verb. `args` are the arguments after
/// [`MODE`]; standard output goes to `out`, `REFUSED:`/`FAILED:` lines and the usage
/// text to `err`. Returns the exit status.
pub fn run(
    host: &dyn AdminHost,
    sudo: &Sudo,
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let (code, why) = match call(host, sudo, args, out) {
        Ok(()) => return 0,
        Err(Stop::Refused(why)) => (2, format!("REFUSED: {why}")),
        Err(Stop::Failed(why)) => (1, format!("FAILED: {why}")),
        Err(Stop::Usage) => (2, USAGE.to_owned()),
    };
    say(err, &why);
    code
}

fn call(
    host: &dyn AdminHost,
    sudo: &Sudo,
    args: &[OsString],
    out: &mut dyn Write,
) -> Result<(), Stop> {
    if host.euid() != 0 {
        return Err(Stop::Refused(
            "run it as root: sudo kbf-mac-session node-admin <verb>".to_owned(),
        ));
    }
    let caller = sudo.caller()?;
    let paths = Paths::under(host.root());
    check_tree(host, &paths)?;
    sweep(host, &paths.state);
    if role_user::check_at_dir(host, &paths).is_ok() {
        sweep(host, &role_user::at_dir(&paths));
    }
    log_invocation(host, &paths, &caller, args)?;
    let verb = Verb::parse(args)?;
    let _lock = if verb.changes() {
        Some(take_lock(host, &paths, out)?)
    } else {
        None
    };
    match verb {
        Verb::Phase0Status => phase0::status(host, &paths, out),
        Verb::Phase0Apply => phase0::apply(host, &paths, out),
        Verb::Phase0Revert => phase0::revert(host, &paths, out),
        Verb::RoleUserStatus => role_user::status(host, &paths, out),
        Verb::RoleUserCreate => role_user::create(host, &paths, out),
    }
}

/// Writes one line; a closed output does not stop the call.
pub(crate) fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
}

/// `result`, or a failure naming `path`.
pub(crate) fn at<T>(path: &Path, result: io::Result<T>) -> Result<T, Stop> {
    result.map_err(|why| Stop::Failed(format!("{}: {why}", path.display())))
}

/// `path`'s [`Meta`] through the host.
pub(crate) fn meta(host: &dyn AdminHost, path: &Path) -> Result<Option<Meta>, Stop> {
    at(path, host.lstat(path))
}

/// `path` is a `want` (never a link), owned by root, writable by root alone.
fn root_owned(host: &dyn AdminHost, path: &Path, want: Kind) -> Result<(), Stop> {
    let refused = |why: String| Err(Stop::Refused(format!("{} {why}", path.display())));
    let Some(meta) = meta(host, path)? else {
        return refused(format!("is not {want} (it is missing)"));
    };
    if meta.kind != want {
        return refused(format!("is not {want} (it is {})", meta.kind));
    }
    if meta.uid != 0 {
        return refused(format!("is owned by uid {}, not root", meta.uid));
    }
    if meta.mode & GROUP_OTHER_WRITE != 0 {
        return refused(format!(
            "is writable by group or others (mode {:o})",
            meta.mode
        ));
    }
    if at(path, host.has_acl(path))? {
        return refused(
            "has an access control list; its mode bits do not say who can write it".to_owned(),
        );
    }
    Ok(())
}

/// `path` is a directory owned by root, writable by root alone.
pub(crate) fn root_dir(host: &dyn AdminHost, path: &Path) -> Result<(), Stop> {
    root_owned(host, path, Kind::Directory)
}

/// `path` is a regular file owned by root, writable by root alone.
pub(crate) fn root_file(host: &dyn AdminHost, path: &Path) -> Result<(), Stop> {
    root_owned(host, path, Kind::File)
}

/// The tree the mode works in, and the running binary with every directory above it.
fn check_tree(host: &dyn AdminHost, paths: &Paths) -> Result<(), Stop> {
    for dir in paths.tree() {
        root_dir(host, &dir)?;
    }
    let exe = at(Path::new("the running binary"), host.current_exe())?;
    let Ok(below) = exe.strip_prefix(&paths.root) else {
        return Err(Stop::Refused(format!(
            "the running binary {} is not under {}",
            exe.display(),
            paths.root.display()
        )));
    };
    let mut dir = paths.root.clone();
    let mut parts = below.components().peekable();
    while let Some(part) = parts.next() {
        dir.push(part);
        if parts.peek().is_some() {
            root_dir(host, &dir)?;
        }
    }
    root_file(host, &exe)
}

/// Removes the helper's temporary files older than [`STALE`] from `dir`.
fn sweep(host: &dyn AdminHost, dir: &Path) {
    let now = host.now();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let stale = entry
            .file_name()
            .as_bytes()
            .starts_with(INCOMING.as_bytes())
            && host.lstat(&path).ok().flatten().is_some_and(|meta| {
                meta.kind == Kind::File
                    && now
                        .duration_since(meta.modified)
                        .is_ok_and(|age| age > STALE)
            });
        if stale {
            let _ = fs::remove_file(&path);
        }
    }
}

/// The audit line of one call: the UTC time, the caller, and the arguments, each
/// cut to [`LOG_ARG_MAX`] bytes and at most [`LOG_ARGS_MAX`] of them, every text
/// quoted with Rust's escapes (`"\n"`, `"\u{1b}"`), so no argument can end the line or
/// write a terminal escape into it.
#[must_use]
pub fn log_line(now: SystemTime, user: &str, uid: u32, args: &[OsString]) -> String {
    let mut line = format!("{} caller={user:?} uid={uid} argv:", utc(now, false));
    for arg in args.iter().take(LOG_ARGS_MAX) {
        let bytes = arg.as_bytes();
        let kept = String::from_utf8_lossy(&bytes[..bytes.len().min(LOG_ARG_MAX)]);
        line.push_str(&format!(" {kept:?}"));
        if bytes.len() > LOG_ARG_MAX {
            line.push_str(&format!("...[{} bytes]", bytes.len()));
        }
    }
    if args.len() > LOG_ARGS_MAX {
        line.push_str(&format!(" ...({} arguments in all)", args.len()));
    }
    line.push('\n');
    line
}

/// `now` in UTC: `2026-10-11T08:00:00Z`, or `20261011T080000Z` when `compact`
/// (for file names).
pub(crate) fn utc(now: SystemTime, compact: bool) -> String {
    let now = time::OffsetDateTime::from(now);
    let (date, sep) = if compact { ("", "") } else { ("-", ":") };
    format!(
        "{:04}{date}{:02}{date}{:02}T{:02}{sep}{:02}{sep}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn log_invocation(
    host: &dyn AdminHost,
    paths: &Paths,
    caller: &Caller,
    args: &[OsString],
) -> Result<(), Stop> {
    let log = &paths.admin_log;
    if let Some(found) = meta(host, log)? {
        root_file(host, log)?;
        if found.size > ADMIN_LOG_MAX {
            at(log, fs::rename(log, log.with_extension("log.1")))?;
        }
    }
    let line = log_line(host.now(), &caller.user, caller.uid, args);
    at(log, append(log, line.as_bytes()))
}

fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)
}

/// The lock a changing verb holds until it returns.
///
/// An `flock` belongs to the open file description, not to the descriptor, and a
/// description lives until every copy of the descriptor is closed. A process forked while
/// the lock is held (any spawn of a child by another thread of this process, as in a
/// test binary that runs verbs on one thread and starts programs on others) carries a
/// copy until it execs, so closing the file would leave the lock held for as long as
/// that takes. Dropping unlocks first, which no copy outlives.
struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // An error leaves the close that follows to release the lock, as before.
        let _ = self.file.unlock();
    }
}

fn take_lock(host: &dyn AdminHost, paths: &Paths, out: &mut dyn Write) -> Result<Lock, Stop> {
    let path = &paths.lock;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path);
    let file = at(path, file)?;
    root_file(host, path)?;
    let mut waited = 0;
    loop {
        let why = match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(Lock { file }),
            Err(why) => why,
        };
        if waited == LOCK_WAIT {
            return Err(Stop::Refused(format!(
                "another kbf-mac-session node-admin call still holds {} after {LOCK_WAIT}s ({why})",
                path.display()
            )));
        }
        if waited == 0 {
            say(
                out,
                "waiting for another kbf-mac-session node-admin call to finish",
            );
        }
        host.pause();
        waited += 1;
    }
}

/// Writes `text` to `target` through a new temporary file in `dir` (mode 0600, synced),
/// renamed over `target`; the temporary file is removed if any step fails.
pub(crate) fn write_new(dir: &Path, target: &Path, text: &str) -> Result<(), Stop> {
    replace(dir, target, text.as_bytes(), |_| Ok(()))
}

/// Like [`write_new`], but the temporary file is given `owner` (uid, gid) and `mode`
/// before it is renamed over `target`.
pub(crate) fn write_new_as(
    host: &dyn AdminHost,
    dir: &Path,
    target: &Path,
    bytes: &[u8],
    owner: (u32, u32),
    mode: u32,
) -> Result<(), Stop> {
    replace(dir, target, bytes, |temp| {
        host.chown(temp, owner.0, owner.1)?;
        fs::set_permissions(temp, fs::Permissions::from_mode(mode))
    })
}

fn replace(
    dir: &Path,
    target: &Path,
    bytes: &[u8],
    finish: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), Stop> {
    let temp = dir.join(format!("{INCOMING}{}", std::process::id()));
    let written = write_temp(&temp, bytes)
        .and_then(|()| finish(&temp))
        .and_then(|()| fs::rename(&temp, target));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    at(target, written)
}

fn write_temp(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Runs a program that only reads: its output, or a failure if it could not start.
pub(crate) fn query(
    host: &dyn AdminHost,
    program: &str,
    args: &[&str],
) -> Result<CommandOutput, Stop> {
    host.command(program, args)
        .map_err(|why| Stop::Failed(format!("{program}: {why}")))
}

/// Runs a program that changes the Mac, printing `+ <command>` first; a failure if it
/// could not start or did not succeed.
pub(crate) fn change(
    host: &dyn AdminHost,
    out: &mut dyn Write,
    program: &str,
    args: &[&str],
) -> Result<(), Stop> {
    let text = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    say(out, &format!("+ {text}"));
    let output = query(host, program, args)?;
    if output.success {
        Ok(())
    } else {
        Err(Stop::Failed(format!(
            "{text} failed: {}",
            output.stderr.trim()
        )))
    }
}
