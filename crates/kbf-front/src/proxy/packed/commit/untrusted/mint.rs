//! Minting the key of a task pack, and checking that a key is one this frontend minted.
//!
//! The frontend issues one key per lease attempt, `cas/b/<lease>/<nonce>-0`, and hands the task a
//! presigned write for it. The nonce is 16 bytes of HMAC-SHA-256 over the lease, the action and
//! the mint minute (the message is [`kbf_pack::mac_input`]) followed by the minute itself, so the
//! janitor reads a pack's age from its key. [`PackKeyMint::verify`] recomputes the MAC from the
//! key's own lease and minute and compares it to the key's in constant time: a key that was not
//! minted under this secret for this action (a made-up nonce, a key of another lease, another
//! action or another frontend's secret) does not verify, and [`super::ingest`] refuses it before
//! it asks the store for a byte.
//!
//! The secret is bytes the caller loads from a file named by a flag, as the server loads its other
//! secrets; this module never reads a file or the environment, and never prints the secret.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use hmac::{Hmac, KeyInit, Mac};
use kbf_pack::{MAC_LEN, TaskKeyError, TaskPackKey, mac_input, minute_of};
use kbf_types::Digest;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::{CommitError, IssuedPackKey};

/// The fewest bytes of secret a [`PackKeyMint`] takes: the length of the MAC's own output, below
/// which the key is weaker than the tag it makes.
pub const MIN_SECRET_BYTES: usize = 32;

/// The slot of the only pack a lease may write: one presigned write per task.
const SLOT: u32 = 0;

/// Why a key could not be minted, or is not one this frontend minted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MintError {
    /// The secret is shorter than [`MIN_SECRET_BYTES`].
    #[error("the secret has {0} bytes, at least {MIN_SECRET_BYTES} are needed")]
    ShortSecret(usize),
    /// The time is before the Unix epoch.
    #[error("the time is before the Unix epoch")]
    Clock,
    /// The lease cannot be a build id, or the time is past the last minute a key can name.
    #[error(transparent)]
    Key(#[from] TaskKeyError),
    /// The key is not an object key.
    #[error("{}", .0.message())]
    Issued(CommitError),
    /// The key was not minted by this frontend for this action.
    #[error("the key {0:?} was not minted here for this action")]
    NotMinted(String),
}

/// The frontend's minting of task pack keys: an HMAC-SHA-256 key and nothing else.
#[derive(Clone)]
pub struct PackKeyMint {
    keyed: Hmac<Sha256>,
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
    /// [`MintError::ShortSecret`] for fewer than [`MIN_SECRET_BYTES`] bytes.
    pub fn new(secret: &[u8]) -> Result<Self, MintError> {
        // HMAC takes a key of any length, so only the length of the secret can refuse it.
        let keyed = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).ok();
        match keyed {
            Some(keyed) if secret.len() >= MIN_SECRET_BYTES => Ok(Self { keyed }),
            _ => Err(MintError::ShortSecret(secret.len())),
        }
    }

    /// The key for the lease `lease` and the action `action`, minted at `now` (slot 0).
    ///
    /// # Errors
    /// [`MintError::Clock`] before the epoch; [`MintError::Key`] for a lease that cannot be a
    /// build id or a time past the last minute a key can name; [`MintError::Issued`] if the
    /// key is not an object key.
    pub fn mint(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
    ) -> Result<IssuedPackKey, MintError> {
        let since = now.duration_since(SystemTime::UNIX_EPOCH);
        let minute = minute_of(since.map_err(|_| MintError::Clock)?.as_secs())?;
        let mac = self.tag(lease, action, minute)?;
        let mut nonce = String::new();
        for byte in mac {
            // Writing to a String does not fail.
            let _ = write!(nonce, "{byte:02x}");
        }
        let _ = write!(nonce, "{minute:08x}-{SLOT}");
        IssuedPackKey::new(lease, &nonce).map_err(MintError::Issued)
    }

    /// Checks that `key` was minted by this frontend, under this secret, for `action`, and says
    /// when: the start of the minute it was minted in. The MAC is compared in constant time.
    ///
    /// # Errors
    /// [`MintError::NotMinted`] for a key that is not of a task pack's form or whose MAC is not
    /// the one this mint computes for its lease, `action` and minute.
    pub fn verify(&self, key: &IssuedPackKey, action: &Digest) -> Result<SystemTime, MintError> {
        let not_minted = || MintError::NotMinted(key.key().to_owned());
        let parsed = TaskPackKey::parse(key.key()).map_err(|_| not_minted())?;
        let wanted = self.tag(parsed.build(), action, parsed.minute())?;
        if !bool::from(wanted.ct_eq(parsed.mac())) {
            return Err(not_minted());
        }
        Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(parsed.minted_unix_secs()))
    }

    /// The first [`MAC_LEN`] bytes of the HMAC over the message of [`mac_input`].
    fn tag(&self, lease: &str, action: &Digest, minute: u32) -> Result<[u8; MAC_LEN], MintError> {
        let mut mac = self.keyed.clone();
        mac.update(&mac_input(lease, action, minute)?);
        let full = mac.finalize().into_bytes();
        Ok(std::array::from_fn(|at| full[at]))
    }
}
