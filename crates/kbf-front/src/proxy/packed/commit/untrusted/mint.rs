//! Minting and checking the key of a task pack (stub).

use std::time::SystemTime;

use kbf_pack::TaskKeyError;
use kbf_types::Digest;

use super::{CommitError, IssuedPackKey};

/// The fewest bytes of secret a [`PackKeyMint`] takes.
pub const MIN_SECRET_BYTES: usize = 32;

/// Why a key could not be minted, or is not one this frontend minted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MintError {
    /// The secret is shorter than [`MIN_SECRET_BYTES`].
    #[error("the secret has {0} bytes, at least {MIN_SECRET_BYTES} are needed")]
    ShortSecret(usize),
    /// The time is before the Unix epoch.
    #[error("the time is before the Unix epoch")]
    Clock,
    /// The lease or the time cannot be part of a key.
    #[error(transparent)]
    Key(#[from] TaskKeyError),
    /// The key cannot be an object key.
    #[error("{}", .0.message())]
    Issued(CommitError),
    /// The key was not minted by this frontend for this action.
    #[error("the key {0:?} was not minted here for this action")]
    NotMinted(String),
}

/// The frontend's minting of task pack keys.
pub struct PackKeyMint {
    _secret: Vec<u8>,
}

impl std::fmt::Debug for PackKeyMint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PackKeyMint(..)")
    }
}

impl PackKeyMint {
    /// A mint over `secret`.
    ///
    /// # Errors
    /// [`MintError::ShortSecret`].
    pub fn new(secret: &[u8]) -> Result<Self, MintError> {
        Ok(Self {
            _secret: secret.to_vec(),
        })
    }

    /// The key for `lease` and `action`, minted at `now`.
    ///
    /// # Errors
    /// [`MintError`].
    pub fn mint(
        &self,
        lease: &str,
        _action: &Digest,
        now: SystemTime,
    ) -> Result<IssuedPackKey, MintError> {
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| MintError::Clock)?
            .as_secs();
        let nonce = format!("{}{:08x}-0", "0".repeat(32), secs / 60);
        IssuedPackKey::new(lease, &nonce).map_err(MintError::Issued)
    }

    /// The start of the minute `key` was minted in.
    ///
    /// # Errors
    /// [`MintError::NotMinted`].
    pub fn verify(&self, key: &IssuedPackKey, _action: &Digest) -> Result<SystemTime, MintError> {
        Err(MintError::NotMinted(key.key().to_owned()))
    }
}
