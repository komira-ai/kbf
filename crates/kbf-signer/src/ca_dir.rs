//! The CA directory at first start.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use kbf_ca::{Authority, Entropy, FileSigner, Policy};

use crate::error::SignerError;

/// What [`ensure`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    /// The directory was missing or empty: a new CA was made in it.
    Created,
    /// The directory held a CA (made by an earlier start, or placed there by the
    /// operator): it was loaded, and nothing was written.
    Loaded,
}

/// The CA of `dir` for the process that owns its key: a new one (`Authority::init_dir`)
/// when `dir` is missing or empty, the existing one when `dir` holds `ca.key` and
/// `ca.pem`. A directory with only one of the two files, or anything else in it, is an
/// error and nothing is written: a half-made CA is never completed over.
///
/// # Errors
/// [`SignerError::Ca`] for a CA that cannot be made or loaded; [`SignerError::CaDirState`]
/// for a directory in neither state.
pub fn ensure(
    dir: &Path,
    policy: Policy,
    now: i64,
    entropy: &mut dyn Entropy,
) -> Result<(Authority, Started), SignerError> {
    let names: Vec<_> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    if names.is_empty() {
        return Ok((
            Authority::init_dir(dir, policy, now, entropy)?,
            Started::Created,
        ));
    }
    let has = |n: &str| names.iter().any(|f| f == n);
    if names.len() == 2 && has(FileSigner::KEY_FILE) && has(Authority::CERT_FILE) {
        return Ok((Authority::load_dir(dir, policy)?, Started::Loaded));
    }
    Err(SignerError::CaDirState)
}
