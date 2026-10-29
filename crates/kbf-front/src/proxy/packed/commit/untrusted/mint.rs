//! Minting the key of a task pack, and checking that a key is one this frontend minted.
//!
//! The frontend issues a key per object of a lease attempt, `cas/b/<lease>/<nonce>-<slot>` (the
//! slot is `<k>` for pack `k` or `L<i>` for large object `i`), and hands the task a presigned
//! write for each. The nonce is 16 bytes of HMAC-SHA-256 over the lease, the action, the mint
//! minute and the slot (the message is [`kbf_pack::mac_input`]) followed by the minute itself, so
//! the janitor reads a key's age from the key. Keys minted before the slot was in the MAC
//! (`-0`, message [`kbf_pack::mac_input_v1`]) still verify, for slot 0 only. [`PackKeyMint::verify`] recomputes the MAC from the
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
use kbf_pack::{
    MAC_LEN, MAX_LARGE, MAX_PACKS, Slot, TaskKeyError, TaskPackKey, mac_input, mac_input_v1,
    minute_of,
};
use kbf_types::Digest;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use super::{CommitError, IssuedPackKey};

/// The fewest bytes of secret a [`PackKeyMint`] takes: the length of the MAC's own output, below
/// which the key is weaker than the tag it makes.
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
    /// The lease cannot be a build id, or the time is past the last minute a key can name.
    #[error(transparent)]
    Key(#[from] TaskKeyError),
    /// The key is not an object key.
    #[error("{}", .0.message())]
    Issued(CommitError),
    /// A key set of no pack, more than [`MAX_PACKS`] packs or more than [`MAX_LARGE`] large objects.
    #[error(
        "a key set of {packs} packs and {large} large objects is not 1..={MAX_PACKS} and 0..={MAX_LARGE}"
    )]
    SetSize {
        /// The packs asked for.
        packs: u8,
        /// The large objects asked for.
        large: u8,
    },
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

    /// The key for `slot` of the lease `lease` and the action `action`, minted at `now`.
    ///
    /// # Errors
    /// [`MintError::Clock`] before the epoch; [`MintError::Key`] for a lease that cannot be a
    /// build id, a slot a run does not have or a time past the last minute a key can name;
    /// [`MintError::Issued`] if the key is not an object key.
    pub fn mint(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
        slot: Slot,
    ) -> Result<IssuedPackKey, MintError> {
        let since = now.duration_since(SystemTime::UNIX_EPOCH);
        let minute = minute_of(since.map_err(|_| MintError::Clock)?.as_secs())?;
        let mac = self.tag(lease, action, minute, slot)?;
        let mut nonce = String::new();
        for byte in mac {
            // Writing to a String does not fail.
            let _ = write!(nonce, "{byte:02x}");
        }
        let _ = write!(nonce, "{minute:08x}-{slot}");
        IssuedPackKey::new(lease, &nonce).map_err(MintError::Issued)
    }

    /// The keys of a run's whole set, minted at one time: `packs` pack keys, slots `0..packs`,
    /// then `large` large keys, slots `L0..L<large>`, in that order.
    ///
    /// # Errors
    /// [`MintError::SetSize`] for no pack, more than [`MAX_PACKS`] or more than [`MAX_LARGE`]
    /// large objects; otherwise those of [`PackKeyMint::mint`].
    pub fn mint_set(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
        packs: u8,
        large: u8,
    ) -> Result<Vec<IssuedPackKey>, MintError> {
        if !(1..=MAX_PACKS).contains(&packs) || large > MAX_LARGE {
            return Err(MintError::SetSize { packs, large });
        }
        let slots = (0..packs)
            .map(Slot::Pack)
            .chain((0..large).map(Slot::Large));
        slots
            .map(|slot| self.mint(lease, action, now, slot))
            .collect()
    }

    /// Checks that `key` was minted by this frontend, under this secret, for `action`, and says
    /// when: the start of the minute it was minted in. The MAC is compared in constant time. A
    /// key verifies when its MAC is the v2 one for its own slot; a key of slot 0 also verifies
    /// when its MAC is the v1 one (minted before the slot was covered), and no other slot does.
    ///
    /// # Errors
    /// [`MintError::NotMinted`] for a key that is not of a task pack's form or whose MAC is not
    /// the one this mint computes for its lease, `action`, minute and slot.
    pub fn verify(&self, key: &IssuedPackKey, action: &Digest) -> Result<SystemTime, MintError> {
        let not_minted = || MintError::NotMinted(key.key().to_owned());
        let parsed = TaskPackKey::parse(key.key()).map_err(|_| not_minted())?;
        let (build, minute, slot) = (parsed.build(), parsed.minute(), parsed.slot());
        let mut matches = self.tag(build, action, minute, slot)?.ct_eq(parsed.mac());
        if slot == Slot::Pack(0) {
            matches |= self.tag_v1(build, action, minute)?.ct_eq(parsed.mac());
        }
        if !bool::from(matches) {
            return Err(not_minted());
        }
        Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(parsed.minted_unix_secs()))
    }

    /// The first [`MAC_LEN`] bytes of the HMAC over the message of [`mac_input`].
    fn tag(
        &self,
        lease: &str,
        action: &Digest,
        minute: u32,
        slot: Slot,
    ) -> Result<[u8; MAC_LEN], MintError> {
        Ok(self.truncated(&mac_input(lease, action, minute, slot)?))
    }

    /// The same over the message of [`mac_input_v1`].
    fn tag_v1(
        &self,
        lease: &str,
        action: &Digest,
        minute: u32,
    ) -> Result<[u8; MAC_LEN], MintError> {
        Ok(self.truncated(&mac_input_v1(lease, action, minute)?))
    }

    fn truncated(&self, message: &[u8]) -> [u8; MAC_LEN] {
        let mut mac = self.keyed.clone();
        mac.update(message);
        let full = mac.finalize().into_bytes();
        std::array::from_fn(|at| full[at])
    }
}
