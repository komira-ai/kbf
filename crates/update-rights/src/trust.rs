//! Reading a file the host trusts: owned by the trusted account, not writable by anyone else.

use std::io;
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

/// Reads a regular file; not yet.
pub fn read_trusted(_path: &Path, _trusted_uid: u32) -> Result<Vec<u8>, TrustError> {
    Err(TrustError::NotRegular)
}

/// Checks a directory; not yet.
pub fn check_dir(_dir: &Path, _trusted_uid: u32) -> Result<(), TrustError> {
    Ok(())
}
