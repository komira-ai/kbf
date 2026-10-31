//! The digest that names one exact declaration: the SHA-256 of the bytes of its file.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// A SHA-256 as 64 lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(String);

/// Why a text is not a digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a SHA-256: 64 lowercase hex digits")]
pub struct BadDigest(String);

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Digest {
        Digest(hex::encode(Sha256::digest(bytes)))
    }

    /// Reads the text `sha256sum` prints.
    pub fn parse(text: &str) -> Result<Digest, BadDigest> {
        let ok = (text.len() == 64) & text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(Digest(text.to_owned()))
        } else {
            Err(BadDigest(text.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
