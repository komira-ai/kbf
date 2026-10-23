//! Who may have written a file this program reads as root.
//!
//! A file is trusted when it is a regular file owned by the trusted user (root) and not
//! writable by its group or by others, and so is every directory above it, all the way up to
//! `/` (whoever can write a directory can replace what is in it). Config and source files are
//! opened with `O_NOFOLLOW` and checked on the open descriptor, so the file checked is the
//! file read.

use std::fs::{self, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

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
    #[error("not a directory")]
    NotDirectory,
    #[error("not owned by the trusted user")]
    NotOwned,
    #[error("writable by its group or by others")]
    Writable,
    #[error("larger than {MAX_FILE_LEN} bytes")]
    TooLarge,
    #[error("a directory above it is not owned by the trusted user")]
    ParentNotOwned,
    #[error("a directory above it is writable by its group or by others")]
    ParentWritable,
}

/// The user whose files are trusted.
#[derive(Debug, Clone)]
pub struct Trust {
    uid: u32,
    /// The highest directory whose ownership and mode are checked (the last one of the
    /// walk upward): `/` for root's.
    anchor: PathBuf,
}

impl Trust {
    /// Root's files.
    pub fn root() -> Self {
        Self {
            uid: 0,
            anchor: PathBuf::from("/"),
        }
    }

    /// Root's files, trusting the directories above a path only up to `anchor` (the last
    /// one checked): a tree such as `/usr/local/kbf` that must be root's from its top down.
    pub fn root_below(anchor: &Path) -> Self {
        Self {
            uid: 0,
            anchor: anchor.to_path_buf(),
        }
    }

    /// Another user's files, for tests (they cannot create files owned by root).
    #[cfg(test)]
    pub fn owned_by(uid: u32, below: &Path) -> Self {
        Self {
            uid,
            anchor: fs::canonicalize(below).unwrap_or_else(|_| below.to_path_buf()),
        }
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

    /// Check every directory above `path`, from the one that holds it up to the anchor
    /// (`/` for root's): each must be owned by the trusted user and not writable by its group
    /// or by others, or whoever can write it can replace the file whatever the file's own
    /// mode is. The path is resolved first (`..` and links), so the directories checked are
    /// the ones the file really lies under, a link's target's included.
    ///
    /// # Errors
    /// [`TrustError`] for a path that cannot be resolved or a directory that is not trusted.
    fn check_parents(&self, path: &Path) -> Result<(), TrustError> {
        let real = fs::canonicalize(path)?;
        for dir in real.ancestors().skip(1) {
            let meta = fs::metadata(dir)?;
            if !self.owns(&meta) {
                return Err(TrustError::ParentNotOwned);
            }
            if meta.mode() & 0o022 != 0 {
                return Err(TrustError::ParentWritable);
            }
            if dir == self.anchor {
                break;
            }
        }
        Ok(())
    }

    /// Check the file at `path`, following a symbolic link to its target.
    ///
    /// # Errors
    /// [`TrustError`] for a file that is missing or not trusted.
    pub fn check_file(&self, path: &Path) -> Result<(), TrustError> {
        self.check_meta(&fs::metadata(path)?)?;
        self.check_parents(path)
    }

    /// Check the directory at `path`, following a symbolic link to its target: it and every
    /// directory above it must be owned by the trusted user and closed to its group and
    /// to others.
    ///
    /// # Errors
    /// [`TrustError`] for a directory that is missing or not trusted.
    pub fn check_dir(&self, path: &Path) -> Result<(), TrustError> {
        let meta = fs::metadata(path)?;
        if !meta.is_dir() {
            return Err(TrustError::NotDirectory);
        }
        if !self.owns(&meta) {
            return Err(TrustError::NotOwned);
        }
        if meta.mode() & 0o022 != 0 {
            return Err(TrustError::Writable);
        }
        self.check_parents(path)
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
        self.check_parents(path)?;
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
