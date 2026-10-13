//! The `__chown-dirs` mode of the `kbf-daemon` binary: gives a lease's input root's
//! directories, and only its directories, to an owner, run inside Podman's user
//! namespace (`podman unshare <kbf-daemon> __chown-dirs <uid>:<gid> <dir>`).
//!
//! With the input cache, a lease's input files are hard links to the cache's entries
//! (`docs/design/worker-cache.md#51-container-driver-linux-rootless-podman-hard-links-into-the-overlay-lower`).
//! `podman unshare chown -hR` over the input root would re-own every cached inode, so
//! that an action could write it. `chown` has no type filter, so the driver runs this
//! instead: a walk by descriptor, in the style of `remove`, that opens each directory
//! relative to its parent with `O_NOFOLLOW`, changes its owner, and leaves every other
//! entry (a file, a symlink, a device) as it is. No recursion: a loop over a `Vec` of
//! frames on the heap, one descriptor held at a time, the way back to a parent through
//! `..`, checked by device and inode.
//!
//! The binary's `main` calls [`dispatch`] before it parses its flags, as for the
//! keeper mode. The argument list is an interface between daemon versions: a driver
//! may run a newer binary after an update renamed it over the old one.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt as _;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Dir, FileType, Gid, Mode, Uid};

use crate::outputs::{DIRECTORY, Identity, back_to_parent, identity};

/// The argument after argv0 that selects this mode.
pub const ARG: &str = "__chown-dirs";

/// The exit status of this mode when `std::env::args_os` asks for it; `None` otherwise.
#[must_use]
pub fn dispatch() -> Option<u8> {
    let args: Vec<OsString> = std::env::args_os().collect();
    (args.get(1).map(OsString::as_os_str) == Some(OsStr::new(ARG))).then(|| run(&args[2..]))
}

/// Makes a test binary answer in the `__chown-dirs` mode when run with it, before its
/// test harness starts: for the driver's tests that run real Podman, whose helper is
/// the test binary itself.
#[macro_export]
macro_rules! chown_dirs_test_hook {
    () => {
        const _: () = {
            // One line: the exit runs only in a process started in the mode, which the
            // coverage job sees no counter of (an earlier test of such a process showed none).
            #[rustfmt::skip]
            extern "C" fn kbf_chown_dirs_hook() {
                if let Some(code) = $crate::chown_dirs::dispatch() { std::process::exit(code.into()) }
            }
            #[used]
            #[unsafe(link_section = ".init_array")]
            static KBF_CHOWN_DIRS_HOOK: extern "C" fn() = kbf_chown_dirs_hook;
        };
    };
}

/// The mode, its arguments after [`ARG`] being `<uid>:<gid> <dir>`: 0 once every
/// directory is changed, 1 when the walk failed, 2 for arguments it cannot read.
#[must_use]
pub fn run(args: &[OsString]) -> u8 {
    let parsed = match args {
        [owner, dir] => owner
            .to_str()
            .and_then(|o| o.split_once(':'))
            .and_then(|(u, g)| Some((u.parse().ok()?, g.parse().ok()?, dir))),
        _ => None,
    };
    let Some((uid, gid, dir)) = parsed else {
        eprintln!("kbf-daemon {ARG}: usage: {ARG} <uid>:<gid> <dir>, not {args:?}");
        return 2;
    };
    let (uid, gid) = (Uid::from_raw(uid), Gid::from_raw(gid));
    match walk(Path::new(dir), &mut |fd| {
        Ok(rustix::fs::fchown(fd, Some(uid), Some(gid))?)
    }) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("kbf-daemon {ARG}: {}: {e}", Path::new(dir).display());
            1
        }
    }
}

/// A directory of the walk.
struct Frame {
    id: Identity,
    /// The subdirectories still to visit.
    subdirectories: Vec<OsString>,
}

/// Calls `each` with a descriptor of `dir` and of every directory below it, not
/// following a symlink. An error stops the walk.
pub(crate) fn walk(
    dir: &Path,
    each: &mut dyn FnMut(&OwnedFd) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut here = rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty())?;
    each(&here)?;
    let mut stack = vec![Frame {
        id: identity(&here)?,
        subdirectories: subdirectories(&here)?,
    }];
    while let Some(mut top) = stack.pop() {
        if let Some(name) = top.subdirectories.pop() {
            let child = rustix::fs::openat(&here, name.as_os_str(), DIRECTORY, Mode::empty())?;
            each(&child)?;
            let frame = Frame {
                id: identity(&child)?,
                subdirectories: subdirectories(&child)?,
            };
            stack.extend([top, frame]);
            here = child;
            continue;
        }
        if let Some(parent) = stack.last() {
            here = back_to_parent(&here, parent.id)?;
        }
    }
    Ok(())
}

/// The names in `dir` that are directories (not symlinks to one).
fn subdirectories(dir: &OwnedFd) -> std::io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsString::from_vec(name);
        let stat = rustix::fs::statat(dir, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            names.push(name);
        }
    }
    Ok(names)
}

#[cfg(test)]
#[path = "chown_dirs_tests.rs"]
mod tests;
