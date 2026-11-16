//! Reading the Action and the Command of a tree job from the job's root, checked, and the rule
//! for the paths a Command names.
//!
//! [`file_message`] reads one message blob from a file of the tree (`.kbf/action`,
//! `.kbf/command`). Nothing the file holds is trusted: the digest's size is checked against
//! [`MAX_MESSAGE_BYTES`] before the file is opened, at most one byte past the size is read, and
//! the bytes are checked against the digest's length and SHA-256 before they are decoded.
//!
//! [`check_relative`] is the rule for a path a Command names (its working directory, an output
//! path).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use kbf_proto::reapi::Digest;
use prost::Message;
use sha2::{Digest as _, Sha256};

/// Why a job's Action or Command could not be read, or the tree could not be used.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// A digest is not a lowercase SHA-256 hex string with a non-negative size.
    #[error("invalid digest {0:?}")]
    BadDigest(String),
    /// The bytes are not the blob their digest names.
    #[error("blob {digest} does not match its digest: {why}")]
    Mismatch {
        /// The digest asked for, as `<hex>/<size>`.
        digest: String,
        /// How the bytes differ.
        why: String,
    },
    /// The tree breaks a rule: the client's error.
    #[error("{0}")]
    Invalid(String),
    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path that was being read.
        path: PathBuf,
        /// The operating system's error.
        #[source]
        source: std::io::Error,
    },
}

/// `<hex>/<size>`: how a digest is written in errors and the manifest.
#[must_use]
pub fn label(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
}

/// Refuses a digest that is not 64 lowercase hex digits with a size of at least 0.
fn check_digest(digest: &Digest) -> Result<(), FetchError> {
    let hex = digest.hash.len() == 64
        && digest
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex && digest.size_bytes >= 0 {
        Ok(())
    } else {
        Err(FetchError::BadDigest(label(digest)))
    }
}

/// The most an Action or Command message may be: these are held in memory, so a digest naming
/// more is refused before the file is opened.
pub const MAX_MESSAGE_BYTES: i64 = 4 << 20;

/// The bytes of the message blob `digest` names, read from the file `path`: the size bound
/// first, then the digest, then the file (at most one byte past the digest's size), checked
/// against the digest's length and SHA-256. A mismatch names `path`.
fn message_bytes(path: &Path, digest: &Digest) -> Result<Vec<u8>, FetchError> {
    if digest.size_bytes > MAX_MESSAGE_BYTES {
        return Err(FetchError::Invalid(format!(
            "message blob {} is {} bytes, over the {MAX_MESSAGE_BYTES}-byte limit for a message",
            label(digest),
            digest.size_bytes
        )));
    }
    check_digest(digest)?;
    let io = |source| FetchError::Io {
        path: path.to_owned(),
        source,
    };
    let size = digest.size_bytes.unsigned_abs();
    let file = std::fs::File::open(path).map_err(io)?;
    let mut bytes = Vec::new();
    file.take(size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    let mismatch = |why: String| FetchError::Mismatch {
        digest: label(digest),
        why: format!("{why}, as read from {}", path.display()),
    };
    let seen = bytes.len() as u64;
    if seen > size {
        return Err(mismatch(format!("it is longer than {size} bytes")));
    }
    if seen != size {
        return Err(mismatch(format!("it has {seen} bytes")));
    }
    let hash = hex::encode(Sha256::digest(&bytes));
    if hash != digest.hash {
        return Err(mismatch(format!("its SHA-256 is {hash}")));
    }
    Ok(bytes)
}

/// Reads the message blob `digest` names from the file `path` (a file of the job's root) and
/// decodes it as `M`.
///
/// # Errors
/// The digest names more than [`MAX_MESSAGE_BYTES`] or the bytes do not decode
/// ([`FetchError::Invalid`]), the digest is malformed ([`FetchError::BadDigest`]), the file
/// cannot be opened or read ([`FetchError::Io`]), or its bytes are not the blob
/// ([`FetchError::Mismatch`]).
pub fn file_message<M: Message + Default>(path: &Path, digest: &Digest) -> Result<M, FetchError> {
    let bytes = message_bytes(path, digest)?;
    M::decode(bytes.as_slice()).map_err(|error| {
        FetchError::Invalid(format!("blob {} does not decode: {error}", label(digest)))
    })
}

/// Checks a path the Command names (working directory, output path): relative, made of
/// non-empty components other than `.` and `..`, joined by single slashes. The empty path is the
/// directory itself.
///
/// # Errors
/// [`FetchError::Invalid`], naming `what`.
pub fn check_relative(what: &str, path: &str) -> Result<(), FetchError> {
    let plain = path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    if !plain {
        return Err(FetchError::Invalid(format!(
            "{what} {path:?} must be relative and must not contain . or .."
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;
