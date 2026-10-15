//! How a lease gets a tree: mounted by the driver (Linux), or cloned whole into the
//! lease's input root (macOS, which has no bind or overlay mount).
//!
//! `clonefile(2)` of a directory clones the whole hierarchy in one call, sharing the
//! data blocks until either side writes (APFS). The clone is the lease's own: every
//! directory and file in it is a new inode, so the action may write, rename and delete
//! in it and the tree in the store stays as it was.

use std::io;
use std::path::Path;
use std::sync::Arc;

use std::os::unix::fs::PermissionsExt as _;

/// Clones the directory `from` (a tree in the store) to the new name `to`: the
/// directory and everything under it, which must not exist yet. Fails `EEXIST` when
/// `to` exists, `ENOENT` when `from` or `to`'s parent is gone, and `ENOTSUP` or `EXDEV`
/// when the two are not on one APFS volume.
pub type CloneDirFn = dyn Fn(&Path, &Path) -> io::Result<()> + Send + Sync;

/// How a tree reaches an action.
#[derive(Clone)]
pub enum Delivery {
    /// The driver mounts the tree read-only at the node's path
    /// ([`super::SharedMount`]); the lease holds an empty directory there.
    Mount,
    /// The tree is cloned into the lease's input root at the node's path, the
    /// function being the clone. A tree it refuses with `ENOTSUP` or `EXDEV` is
    /// written file by file through the cache instead.
    Clone(Arc<CloneDirFn>),
}

impl std::fmt::Debug for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Mount => "Mount",
            Self::Clone(_) => "Clone",
        })
    }
}

impl Delivery {
    /// What the platform does: clones on macOS, a mount elsewhere.
    #[must_use]
    pub fn platform() -> Self {
        platform()
    }
}

#[cfg(target_os = "macos")]
fn platform() -> Delivery {
    Delivery::Clone(Arc::new(clone_dir))
}

#[cfg(not(target_os = "macos"))]
fn platform() -> Delivery {
    Delivery::Mount
}

/// `clonefile` of the directory `from` to `to` (see [`CloneDirFn`]).
#[cfg(target_os = "macos")]
fn clone_dir(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, CloneFlags, fclonefileat};
    let source = std::fs::File::open(from)?;
    fclonefileat(&source, CWD, to, CloneFlags::empty())?;
    Ok(())
}

/// What a clone attempt came to.
#[derive(Debug)]
pub(super) enum Placed {
    /// `to` is a clone of the tree, moded as inputs are without the cache.
    Cloned,
    /// The filesystem refused (`ENOTSUP`, `EXDEV`): `to` is an empty directory again.
    Refused(io::Error),
}

/// Whether `error` is the filesystem saying it cannot clone this.
fn refused(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EXDEV))
}

/// Replaces the empty directory `at` (the mount point the walk made) with a clone of
/// the tree `source`, then gives the clone the modes an input has without the cache:
/// files 0644, or 0755 when the tree's is executable (a tree's files are 0444 and 0555,
/// so the store's tree is never writable); directories are 0755 already. Blocking.
///
/// # Errors
/// `at` cannot be replaced, or the clone failed for a reason other than a refusal, or
/// a clone's file cannot be moded.
pub(super) fn place(clone: &CloneDirFn, source: &Path, at: &Path) -> io::Result<Placed> {
    std::fs::remove_dir(at)?;
    match clone(source, at) {
        Ok(()) => {
            writable(at)?;
            Ok(Placed::Cloned)
        }
        Err(e) if refused(&e) => {
            std::fs::create_dir(at)?;
            Ok(Placed::Refused(e))
        }
        Err(e) => Err(e),
    }
}

/// Gives every file under `root` the mode of an input (never following a link).
fn writable(root: &Path) -> io::Result<()> {
    let mut dirs = vec![root.to_owned()];
    while let Some(here) = dirs.pop() {
        for entry in std::fs::read_dir(&here)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                let executable = entry.metadata()?.permissions().mode() & 0o111 != 0;
                let mode = if executable { 0o755 } else { 0o644 };
                std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}
