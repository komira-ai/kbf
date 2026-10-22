//! Who may have written a file this program reads as root.
//!
//! A file is trusted when it is a regular file owned by the trusted user (root) and not
//! writable by its group or by others. Config and source files are opened with
//! `O_NOFOLLOW` and checked on the open descriptor, so the file checked is the file read.

use std::fs::Metadata;
use std::io;
use std::path::Path;

/// The largest file read as config, in bytes.
pub const MAX_FILE_LEN: u64 = 64 * 1024;

/// Why a file is not trusted.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("cannot read: {0}")]
    Io(#[from] io::Error),
    #[error("not a regular file")]
    NotRegular,
    #[error("not owned by the trusted user")]
    NotOwned,
    #[error("writable by its group or by others")]
    Writable,
    #[error("larger than {MAX_FILE_LEN} bytes")]
    TooLarge,
}

/// The user whose files are trusted.
#[derive(Debug, Clone, Copy)]
pub struct Trust {
    uid: u32,
}

impl Trust {
    /// Root's files.
    pub const fn root() -> Self {
        Self { uid: 0 }
    }

    /// Another user's files, for tests (they cannot create files owned by root).
    #[cfg(test)]
    pub const fn owned_by(uid: u32) -> Self {
        Self { uid }
    }

    /// Whether `meta` is owned by the trusted user.
    pub fn owns(&self, meta: &Metadata) -> bool {
        let _ = meta;
        todo!()
    }

    /// Check `meta` of a regular file: owner and mode.
    ///
    /// # Errors
    /// [`TrustError`] for a file that is not trusted.
    pub fn check_meta(&self, meta: &Metadata) -> Result<(), TrustError> {
        let _ = meta;
        todo!()
    }

    /// Check the file at `path`, following a symbolic link to its target.
    ///
    /// # Errors
    /// [`TrustError`] for a file that is missing or not trusted.
    pub fn check_file(&self, path: &Path) -> Result<(), TrustError> {
        let _ = path;
        todo!()
    }

    /// Read the file at `path` as text: opened without following a symbolic link, checked
    /// on the open descriptor, read up to [`MAX_FILE_LEN`].
    ///
    /// # Errors
    /// [`TrustError`] for a file that is missing, not trusted, too large or not UTF-8.
    pub fn read(&self, path: &Path) -> Result<String, TrustError> {
        let _ = path;
        todo!()
    }
}

#[cfg(test)]
mod tests;
