//! Reading a file the host trusts: owned by the trusted account, not writable by anyone else.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// Why a file or directory is not trusted.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("not a regular file")]
    NotRegular,
    #[error("not a directory")]
    NotDirectory,
    #[error("owned by uid {found}, not the trusted uid {wanted}")]
    Owner { found: u32, wanted: u32 },
    #[error("mode {mode:o} lets the group or others write it")]
    Writable { mode: u32 },
}

fn judge(meta: &std::fs::Metadata, trusted_uid: u32) -> Result<(), TrustError> {
    if meta.uid() != trusted_uid {
        return Err(TrustError::Owner {
            found: meta.uid(),
            wanted: trusted_uid,
        });
    }
    let mode = meta.mode();
    if mode & 0o022 != 0 {
        return Err(TrustError::Writable {
            mode: mode & 0o7777,
        });
    }
    Ok(())
}

/// Reads a regular file that `trusted_uid` owns and that only its owner can write. The file is
/// opened without following a link or waiting on a pipe, and the checks are made on the open file, so the file that
/// is checked is the file that is read.
pub fn read_trusted(path: &Path, trusted_uid: u32) -> Result<Vec<u8>, TrustError> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(TrustError::NotRegular);
    }
    judge(&meta, trusted_uid)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Checks that `dir` is a directory (not a link to one) that `trusted_uid` owns and only its
/// owner can write.
pub fn check_dir(dir: &Path, trusted_uid: u32) -> Result<(), TrustError> {
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(TrustError::NotDirectory);
    }
    judge(&meta, trusted_uid)
}
