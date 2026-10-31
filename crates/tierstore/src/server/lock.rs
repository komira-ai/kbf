//! The lock file of a tierstore process: an exclusive `flock(2)` on a file, taken without
//! waiting, that decides which of two processes over one directory may write to it.
//!
//! The build node's lock is `<cache>/.lock` (see [`local_cache`](super::local_cache)); a storage
//! node's is the file `--lock-file` names. The process that holds it is the writer; one that
//! finds it held goes on without and tries again later. The kernel frees the lock when the
//! process exits or crashes, so nothing has to release it.
//!
//! A lock is on the file that was opened, not on the name. A file that is removed and made again
//! under the same name is another file, and a second process can lock it while the first still
//! holds the old one, so [`LockFile::is_current`] says whether the name still leads to the file
//! that is held. The file is opened with `O_NOFOLLOW` and `O_CLOEXEC`, and refused on a network
//! file system, where `flock` is not a promise between hosts.

use std::fs::{File, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, openat};

/// A lock file that is held. Dropping it unlocks, and closes the file.
#[derive(Debug)]
pub(super) struct LockFile {
    file: File,
    path: PathBuf,
}

/// Opens the lock file at `path` (making it, mode 0600, if it is not there) and tries the
/// exclusive lock without waiting: `Some` if this call holds it now, `None` if another open
/// file holds it.
///
/// # Errors
/// What the system said about the file: it cannot be opened, it is a link, or it is on a
/// network file system.
pub(super) fn try_take(path: &Path) -> io::Result<Option<LockFile>> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = openat(CWD, path, flags, Mode::RUSR | Mode::WUSR)?;
    let file = File::from(fd);
    check_file_system(&file, path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(why)) => return Err(why),
    }
    let taken = LockFile {
        file,
        path: path.to_owned(),
    };
    // The name may have been given to another file between the open and the lock: a lock on a
    // file nothing else will look at. It is dropped, and the caller tries again.
    Ok(taken.is_current().then_some(taken))
}

/// Why `path` cannot be a lock file, if it cannot: the words that follow its name in a refusal.
/// A relative path would lead to another file when the working directory is another, and two
/// processes of one node must name one file.
pub(super) fn check_lock_path(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute() {
        return Err("must be an absolute path");
    }
    if path.file_name().is_none() {
        return Err("must name a file, not a directory");
    }
    Ok(())
}

impl LockFile {
    /// Whether the name this lock was taken under still leads to the file that is held (the
    /// `fstat` of the descriptor against the `lstat` of the path). `false` if the file was
    /// replaced, turned into a link or removed.
    pub(super) fn is_current(&self) -> bool {
        let identity = |meta: std::fs::Metadata| (meta.dev(), meta.ino());
        let held = self.file.metadata().ok().map(identity);
        let named = std::fs::symlink_metadata(&self.path).ok().map(identity);
        held.is_some_and(|held| Some(held) == named)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Unlocked before the close, so a copy of the descriptor that a forked child still has
        // does not keep the lock. An error leaves the close to release it.
        let _ = self.file.unlock();
    }
}

/// The file system types whose `flock` is not exclusive between processes on other hosts, or
/// not kept at all (the `f_type` magic numbers of `statfs(2)`): NFS, SMB, CIFS, SMB2, Ceph,
/// FUSE (sshfs, glusterfs, virtiofs), Lustre, GPFS, OCFS2, 9p and AFS. A file system with no
/// stable magic number is not in the list.
///
/// The lock file is `<cache>/.lock`, opened without following a link, so the file system of the
/// cache directory is the one of the lock file, and checking the lock file checks both; a
/// storage node's lock file is checked, and nothing else it has is.
// Used by the Linux `check_file_system` and by the tests; unused elsewhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) const NETWORK: [i128; 11] = [
    0x6969,
    0x517B,
    0xFF53_4D42,
    0xFE53_4D42,
    0x00C3_6400,
    0x6573_5546,
    0x0BD0_0BD0,
    0x4750_4653,
    0x7461_636F,
    0x0102_1997,
    0x5346_414F,
];

/// Refuses a lock file on a file system whose `statfs` type is `magic`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn check_type(magic: i128, path: &Path) -> io::Result<()> {
    if NETWORK.contains(&magic) {
        let why = format!(
            "{} is on a network file system (FUSE and cluster file systems included); a lock \
             file must be on a local one",
            path.display()
        );
        return Err(io::Error::other(why));
    }
    Ok(())
}

/// Asks the file system of `file` what it is, and refuses a network one.
#[cfg(target_os = "linux")]
fn check_file_system(file: &File, path: &Path) -> io::Result<()> {
    let magic = rustix::fs::fstatfs(file)?.f_type;
    check_type(i128::from(magic), path)
}

/// Other systems have no `f_type` to ask; the lock is taken as it is.
#[cfg(not(target_os = "linux"))]
fn check_file_system(_file: &File, _path: &Path) -> io::Result<()> {
    Ok(())
}
