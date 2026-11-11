//! kbf's own rule for the object keys it names and the names it builds them from.
//!
//! The store's key types ([`ObjectKey`], [`KeyPrefix`]) take S3's rules: any UTF-8 text of 1 to
//! 1024 bytes with no `.` or `..` segment. kbf asks more of a key it makes from a name it was
//! given (a node, a boot, a build, a pack key a node commits, a prefix of a flag or a URI): only
//! ASCII letters, digits, `-`, `_`, `.`, `~` and `/` as a separator, and no empty, `.` or `..`
//! segment. A name that passes needs no escaping anywhere, two names never make one key, and a
//! key under a prefix stays under it whatever a store or a URL does to it.
//!
//! [`kbf_key`] and [`kbf_prefix`] check that rule and make the store's type.

use crate::{KeyError, KeyPrefix, ObjectKey};

/// Why a key or prefix is not one kbf names.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KbfKeyError {
    /// The store's own rule refuses it: empty, or longer than 1024 bytes.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// A character outside `A-Z a-z 0-9 - _ . ~ /`.
    #[error("object key {key:?} has {ch:?}; only A-Z a-z 0-9 - _ . ~ / are allowed")]
    Character {
        /// The refused key.
        key: String,
        /// The first refused character.
        ch: char,
    },
    /// A `/`-separated segment that is empty, `.` or `..`.
    #[error("object key {0:?} has an empty, `.` or `..` segment")]
    Segment(String),
}

/// `key` as an [`ObjectKey`], if it is one kbf names (see the module).
///
/// # Errors
/// [`KbfKeyError`]: the store's refusal (empty, too long), a character outside the alphabet, or
/// an empty, `.` or `..` segment.
pub fn kbf_key(key: impl Into<String>) -> Result<ObjectKey, KbfKeyError> {
    let key = key.into();
    let object = ObjectKey::new(key.as_str()).map_err(|error| match error {
        KeyError::Segment(key) => KbfKeyError::Segment(key),
        other => KbfKeyError::Key(other),
    })?;
    if let Some(ch) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/')))
    {
        return Err(KbfKeyError::Character { key, ch });
    }
    // The store refused `.` and `..` already; an empty segment is kbf's alone.
    if key.split('/').any(str::is_empty) {
        return Err(KbfKeyError::Segment(key));
    }
    Ok(object)
}

/// `prefix` as a [`KeyPrefix`], if it is one kbf names: empty, or a key kbf names (see
/// [`kbf_key`]) with or without a `/` after it, so that every key made under it is one too.
///
/// # Errors
/// [`KbfKeyError`]: the store's refusal (too long), or the key part's.
pub fn kbf_prefix(prefix: impl Into<String>) -> Result<KeyPrefix, KbfKeyError> {
    let prefix = prefix.into();
    let made = KeyPrefix::new(prefix.as_str())?;
    if !prefix.is_empty() {
        kbf_key(prefix.strip_suffix('/').unwrap_or(&prefix))?;
    }
    Ok(made)
}
