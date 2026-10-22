//! Who may have written a file this program reads as root.
//!
//! A file is trusted when it is a regular file owned by the trusted user (root) and not
//! writable by its group or by others. Config and source files are opened with
//! `O_NOFOLLOW` and checked on the open descriptor, so the file checked is the file read.

use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The largest file read as config, in bytes.
pub const MAX_FILE_LEN: u64 = 64 * 1024;

/// The `open(2)` flag that refuses a final symbolic link, for `custom_flags`.
pub fn nofollow() -> i32 {
    // One bit of the flags word, far below the sign bit.
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}

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
        meta.uid() == self.uid
    }

    /// Check `meta` of a regular file: owner and mode.
    ///
    /// # Errors
    /// [`TrustError`] for a file that is not trusted.
    pub fn check_meta(&self, meta: &Metadata) -> Result<(), TrustError> {
        if !meta.is_file() {
            return Err(TrustError::NotRegular);
        }
        if !self.owns(meta) {
            return Err(TrustError::NotOwned);
        }
        if meta.mode() & 0o022 != 0 {
            return Err(TrustError::Writable);
        }
        Ok(())
    }

    /// Check the file at `path`, following a symbolic link to its target.
    ///
    /// # Errors
    /// [`TrustError`] for a file that is missing or not trusted.
    pub fn check_file(&self, path: &Path) -> Result<(), TrustError> {
        self.check_meta(&fs::metadata(path)?)
    }

    /// Read the file at `path` as text: opened without following a symbolic link, checked
    /// on the open descriptor, read up to [`MAX_FILE_LEN`].
    ///
    /// # Errors
    /// [`TrustError`] for a file that is missing, not trusted, too large or not UTF-8.
    pub fn read(&self, path: &Path) -> Result<String, TrustError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nofollow())
            .open(path)?;
        self.check_meta(&file.metadata()?)?;
        let mut text = String::new();
        file.take(MAX_FILE_LEN + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_FILE_LEN {
            return Err(TrustError::TooLarge);
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests;
