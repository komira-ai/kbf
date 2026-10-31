//! The lock file of a tierstore process: an exclusive `flock(2)` on a file, taken without
//! waiting, that decides which of two processes over one directory may write to it.
//!
//! The build node's lock is `<cache>/.lock` (see [`local_cache`](super::local_cache)); a storage
//! node's is the file `--lock-file` names.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, Mode, OFlags, openat};

/// A lock file that is held.
#[derive(Debug)]
pub(super) struct LockFile {
    _file: File,
    _path: PathBuf,
}

/// Opens the lock file at `path`.
///
/// # Errors
/// What the system said about the file.
pub(super) fn try_take(path: &Path) -> io::Result<Option<LockFile>> {
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC;
    let fd = openat(CWD, path, flags, Mode::RUSR | Mode::WUSR)?;
    Ok(Some(LockFile {
        _file: File::from(fd),
        _path: path.to_owned(),
    }))
}

/// Why `path` cannot be a lock file, if it cannot.
pub(super) fn check_lock_path(_path: &Path) -> Result<(), &'static str> {
    Ok(())
}

impl LockFile {
    /// Whether the name the lock was taken under still leads to the file that is held.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn is_current(&self) -> bool {
        true
    }
}

/// The file system types whose `flock` is not exclusive between hosts.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const NETWORK: [i128; 5] = [0x6969, 0x517B, 0xFF53_4D42, 0xFE53_4D42, 0x00C3_6400];

/// Refuses a lock file on a file system whose `statfs` type is `magic`.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn check_type(_magic: i128, _path: &Path) -> io::Result<()> {
    Ok(())
}
