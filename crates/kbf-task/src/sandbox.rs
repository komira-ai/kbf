//! The Command's own mount and PID namespaces (Linux): what it sees of the job, and that nothing
//! it started is still running when its outputs are read.
//!
//! # What the Command sees
//!
//! The `unshare` that makes the network layer's namespaces ([`crate::network`]) also makes a
//! mount and a PID namespace. In the mount namespace, after the network layer's steps and before
//! the seccomp filter, the process that becomes the Command ([`crate::child`]) does this, in
//! order ([`layout`]):
//!
//! 1. every mount is made private, so nothing below reaches the namespace of the job;
//! 2. each hidden directory that exists ([`Run::hidden`](crate::run::Run); in a job, the job's
//!    grants at `/run/js-files` and the parent of the task's `--work-dir`, `/scratch` by default,
//!    which holds the input root, the run's `private/` directory and the stdout and stderr
//!    files) is covered by an empty tmpfs;
//! 3. the run's own directories are bound at the system's temporary paths, each one that exists
//!    as a directory: `private/tmp` (the Command's `TMPDIR`) at `/tmp`, `private/var-tmp` at
//!    `/var/tmp`, `private/run` at `/run` and `private/home` (its `HOME`) at `/root`. They are
//!    on the run's scratch disk and executable, where the job's own `/tmp` is a small `noexec`
//!    tmpfs; and `/run` no longer shows `/run/js-files`;
//! 4. the input root and `private/` are bound again at their own paths (each directory on the way
//!    made in whatever now stands there), so the working directory, the program's path and the
//!    variables that name the run's directories still name them;
//! 5. each cover is made read-only, and the working directory is entered again.
//!
//! Each bind's source is opened by the child (`O_PATH`, not following a symlink) right after the
//! `unshare`, before anything covers its path, onto a descriptor number the tool reserved, and is
//! named `/proc/self/fd/<n>` from then on. (A descriptor the tool itself opened cannot be the
//! source: it belongs to the tool's mount namespace, and a bind takes its source from the
//! namespace it is made in.) The Command keeps its standard output and error, which are open
//! files.
//!
//! The Command cannot undo this. It runs as a user other than root in its user namespace, and
//! before it is started the namespace's first process drops every capability and sets
//! `no_new_privs` ([`crate::relay`]), so it holds none there, and neither a set-id program nor a file capability gives it one (a
//! Command allowed the network, which has no seccomp filter, gets `no_new_privs` here). A user
//! namespace it makes of its own gets the mounts locked together, so a cover cannot be taken off
//! what it covers there either.
//!
//! # Nothing outlives the Command
//!
//! The Command is the second process of the PID namespace. The tool starts it through this
//! binary's relay ([`Run::relay`](crate::run::Run), [`crate::relay`]): the first process of the
//! namespace starts it, reaps every process handed to it, and ends when the Command does; the kernel then kills
//! every other process of the namespace, and the end of the first is seen only once they are all
//! gone. So no process of the action runs when its outputs are read, whatever it did with its
//! process group (`setsid` included). The exit status is passed up: the Command's code, or 128
//! plus the signal that ended it. A run with no relay (`Run::relay` is `None`) makes no PID
//! namespace, and its Command is started as it is. The namespace has no `/proc` of its own: the job's is kept
//! (a new one cannot be mounted where the job's runtime has masked parts of it), so the Command
//! sees the job's process ids there, and it can signal none of them.
//!
//! # When the host refuses
//!
//! As with the network namespace, a step that fails does not fail the run: the steps before it
//! hold and none after it is made, and the line the tool writes says which failed:
//! `kbf-task: action <digest>: sandbox: incomplete (<step> <errno>)` ([`Sandbox`]). A host that
//! does not let an unprivileged user namespace keep its capabilities (AppArmor's
//! `apparmor_restrict_unprivileged_userns`) fails at `setgroups` or `uid_map` (`EACCES` or `EPERM`,
//! as the host answers), or at `private` with `EPERM`; the
//! PID namespace is then made (when the `unshare` itself succeeded) and the mounts are not. When
//! the `unshare` fails, the Command runs as it would without this module.
//!
//! # Root
//!
//! A Command run as root gets the mount and PID namespaces without a user namespace (one that maps
//! only uid 0 would make `chown` to another user fail). It keeps `CAP_SYS_ADMIN` there, so the
//! covers do not bind it: it could unmount them. A job runs as a user other than root.

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

use kbf_types::Digest;

use crate::run::RunError;

/// Where the job's host writes the job's files (its grants), read-only.
pub const GRANTS_DIR: &str = "/run/js-files";

/// The run's own directories under its `private/` directory, and the system path each is bound
/// at.
pub const BOUND: [(&str, &str); 4] = [
    ("tmp", "/tmp"),
    ("var-tmp", "/var/tmp"),
    ("run", "/run"),
    ("home", "/root"),
];

/// How the Command's sandbox came out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sandbox {
    /// Every step was made: the hidden directories are covered, the run's directories are bound,
    /// and the Command is the second process of its own PID namespace.
    Made,
    /// A step failed; the steps before it hold, none after it was made.
    Incomplete {
        /// The step: `unshare`, `setgroups`, `uid_map`, `gid_map`, `loopback`, `dumpable`,
        /// `private`, `cover`, `bind`, `mkdir`, `keep`, `seal` or `chdir`.
        step: &'static str,
        /// Its `errno`.
        errno: i32,
    },
    /// The child did not report.
    Unreported,
    /// There is no sandbox on this system (it is built for Linux only).
    Unavailable,
}

impl std::fmt::Display for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sandbox::Made => write!(f, "mount and pid namespaces"),
            Sandbox::Incomplete { step, errno } => write!(f, "incomplete ({step} {errno})"),
            Sandbox::Unreported => write!(f, "unknown (no report from the child)"),
            Sandbox::Unavailable => write!(f, "none (Linux only)"),
        }
    }
}

/// The line written to standard error once the Command has been started, naming how its
/// sandbox came out; none where there is no sandbox ([`Sandbox::Unavailable`]).
#[must_use]
pub fn notice(action: &Digest, sandbox: &Sandbox) -> Option<String> {
    (*sandbox != Sandbox::Unavailable)
        .then(|| format!("kbf-task: action {action}: sandbox: {sandbox}"))
}

/// The directories a task hides from its Command: the job's grants, and the parent of `work_dir`
/// (the task's scratch, `/scratch` for the default `/scratch/work`), or `work_dir` itself when
/// its parent is `/` or it has none.
#[must_use]
pub fn hidden_for(work_dir: &Path) -> Vec<PathBuf> {
    let scratch = work_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty() && *parent != Path::new("/"))
        .unwrap_or(work_dir);
    vec![PathBuf::from(GRANTS_DIR), scratch.to_owned()]
}

/// The sandbox's mount steps for one run. The steps that open a bind's source hold the
/// descriptor it is moved onto, which the parent reserved.
#[derive(Debug, Default)]
pub struct Layout {
    #[cfg(target_os = "linux")]
    steps: Vec<(u8, crate::child::Step)>,
}

#[cfg(target_os = "linux")]
impl Layout {
    /// The steps.
    pub(crate) fn into_steps(self) -> Vec<(u8, crate::child::Step)> {
        self.steps
    }
}

/// The mount steps that give a Command run in `work_dir` (inside `root`, the input root) its
/// view: `hidden` covered (each that is a directory, but `/`), the run's directories under
/// `private` bound at [`BOUND`] (each target that is a directory), `root` and `private` bound
/// again at their own paths where a cover or a bound target hides them, the covers made
/// read-only and `work_dir` entered again. See the module documentation. `root` and `private`
/// must exist.
///
/// # Errors
/// [`RunError::Io`]: `root` or `private` cannot be resolved, a descriptor cannot be reserved,
/// or `work_dir` cannot be made absolute; [`RunError::Invalid`]: a path holds a NUL byte.
#[cfg(target_os = "linux")]
pub fn layout(
    hidden: &[PathBuf],
    private: &Path,
    root: &Path,
    work_dir: &Path,
) -> Result<Layout, RunError> {
    layout_with(&BOUND, hidden, private, root, work_dir)
}

/// [`layout`] with the run's directories bound at `bound` instead of [`BOUND`], so a test can
/// name targets of its own.
#[cfg(target_os = "linux")]
fn layout_with(
    bound_targets: &[(&str, &str)],
    hidden: &[PathBuf],
    private: &Path,
    root: &Path,
    work_dir: &Path,
) -> Result<Layout, RunError> {
    use crate::child::{BIND, CHDIR, COVER, PRIVATE, SEAL, Step};

    // Real paths throughout (a mount resolves its target's symlinks), so that a directory is
    // known to be under a cover or a bound target by its path alone.
    let covered: Vec<PathBuf> = hidden
        .iter()
        .filter_map(|dir| real_dir(dir))
        .filter(|dir| dir != Path::new("/"))
        .collect();
    // A target that is another's real path (`/var/tmp` a symlink to `/tmp`) is bound once, by
    // the first: a second bind would hide the directory `TMPDIR` names.
    let mut bound: Vec<(&str, PathBuf)> = Vec::new();
    for (sub, target) in bound_targets {
        if let Some(target) = real_dir(Path::new(target))
            && !bound.iter().any(|(_, seen)| *seen == target)
        {
            bound.push((sub, target));
        }
    }
    let targets: Vec<PathBuf> = bound.iter().map(|(_, target)| target.clone()).collect();
    let private = real(private)?;
    let keeps = [real(root)?, private.clone()];

    let mut built = Built::default();
    for dir in &covered {
        built.steps.push((COVER, Step::Cover(c_path(dir)?)));
    }
    // A cover is written to only here, to make the mount points of what it must not hide.
    for keep in &keeps {
        if under(keep, &covered) {
            built.keep_at(keep)?;
        }
    }
    for dir in &covered {
        built.steps.push((SEAL, Step::Seal(c_path(dir)?)));
    }
    for (sub, target) in &bound {
        let from = built.source(&private.join(sub))?;
        let to = c_path(target)?;
        built.steps.push((BIND, Step::Bind { from, to }));
    }
    // A directory of the run under one of those paths (where a test keeps its scratch) is bound
    // again inside the run's own directory that now stands there.
    for keep in &keeps {
        if under(keep, &targets) {
            built.keep_at(keep)?;
        }
    }
    built
        .steps
        .push((CHDIR, Step::Chdir(c_path(&absolute(work_dir)?)?)));
    // The sources are opened first, before anything covers their paths.
    let mut steps = vec![(PRIVATE, Step::Private)];
    steps.append(&mut built.holds);
    steps.append(&mut built.steps);
    Ok(Layout { steps })
}

/// The steps being built: the sources to open, and the rest.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Built {
    holds: Vec<(u8, crate::child::Step)>,
    steps: Vec<(u8, crate::child::Step)>,
}

#[cfg(target_os = "linux")]
impl Built {
    /// The path that names `dir` in the child once it is held: a descriptor number is reserved
    /// here (a descriptor of `/dev/null`, closing on `exec`), and the child opens `dir` onto it.
    fn source(&mut self, dir: &Path) -> Result<std::ffi::CString, RunError> {
        use std::os::fd::AsRawFd as _;
        let null = Path::new("/dev/null");
        let reserved = OwnedFd::from(std::fs::File::open(null).map_err(io(null))?);
        let fd = reserved.as_raw_fd();
        let named = c_path(Path::new(&format!("/proc/self/fd/{fd}")))?;
        self.holds.push((
            crate::child::HOLD,
            crate::child::Step::Hold {
                dir: c_path(dir)?,
                fd: reserved,
            },
        ));
        Ok(named)
    }

    /// The steps that bind `keep` again at its own path: each directory on the way made (one
    /// that exists is no failure), then the bind.
    fn keep_at(&mut self, keep: &Path) -> Result<(), RunError> {
        use crate::child::{KEEP, MKDIR, Step};
        let mut on_the_way: Vec<&Path> = keep.ancestors().collect();
        on_the_way.reverse();
        // The first is `/`, which exists.
        for dir in on_the_way.into_iter().skip(1) {
            self.steps.push((MKDIR, Step::Mkdir(c_path(dir)?)));
        }
        let from = self.source(keep)?;
        let to = c_path(keep)?;
        self.steps.push((KEEP, Step::Bind { from, to }));
        Ok(())
    }
}

/// Whether `dir` is one of `dirs` or below one.
#[cfg(target_os = "linux")]
fn under(dir: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|above| dir.starts_with(above))
}

/// The real path of `dir` when it is a directory; `None` when it is anything else or absent.
#[cfg(target_os = "linux")]
fn real_dir(dir: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(dir).ok().filter(|real| real.is_dir())
}

/// An I/O error at `path` as the run's error.
#[cfg(target_os = "linux")]
fn io(path: &Path) -> impl FnOnce(std::io::Error) -> RunError + '_ {
    move |source| RunError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The real path of `dir`, which must exist.
#[cfg(target_os = "linux")]
fn real(dir: &Path) -> Result<PathBuf, RunError> {
    std::fs::canonicalize(dir).map_err(io(dir))
}

/// No sandbox is built on this system: no steps.
///
/// # Errors
/// None here.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)] // The Linux form can fail.
pub fn layout(
    _hidden: &[PathBuf],
    _private: &Path,
    _root: &Path,
    _work_dir: &Path,
) -> Result<Layout, RunError> {
    Ok(Layout::default())
}

#[cfg(target_os = "linux")]
fn absolute(path: &Path) -> Result<PathBuf, RunError> {
    std::path::absolute(path).map_err(io(path))
}

#[cfg(target_os = "linux")]
fn c_path(path: &Path) -> Result<std::ffi::CString, RunError> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| RunError::Invalid(format!("{} holds a NUL byte", path.display())))
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
