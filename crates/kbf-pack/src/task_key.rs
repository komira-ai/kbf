//! The key of a task pack: `cas/b/<build>/<nonce>-<slot>`.
//!
//! The frontend mints one key per lease attempt and hands the task a presigned write for it;
//! the task never picks a key. The nonce is 32 hex digits of a MAC (the frontend's HMAC over
//! the lease, the action and the mint minute, truncated to 16 bytes) followed by the mint
//! minute in 8 hex digits, so the janitor reads a pack's age from its key alone. This module
//! is the shape of that key and the message the MAC covers; the secret, the HMAC and the
//! comparison against a presented key belong to the frontend that mints.

use std::fmt;

use kbf_types::Digest;

/// The prefix every task pack key starts with.
const PREFIX: &str = "cas/b/";

/// The domain tag at the start of the MAC's message.
const MAC_DOMAIN: &[u8] = b"kbf-task-pack/v1";

/// The bytes of the MAC a key carries (a truncated HMAC-SHA-256).
pub const MAC_LEN: usize = 16;

/// The longest build id a key or a manifest may hold.
pub const MAX_BUILD_BYTES: usize = 128;

/// Why a string is not a task pack key, or a part of one is not allowed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TaskKeyError {
    /// The build id is empty.
    #[error("the build id is empty")]
    EmptyBuild,
    /// The build id is longer than [`MAX_BUILD_BYTES`].
    #[error("the build id has {0} bytes, more than {MAX_BUILD_BYTES}")]
    LongBuild(usize),
    /// The build id holds a character outside `0-9 a-z A-Z - _` (a `/` among them).
    #[error("the build id holds {0:?}; only 0-9, a-z, A-Z, - and _ are allowed")]
    BuildCharacter(char),
    /// The string is not `cas/b/<build>/<nonce>` with no further `/`.
    #[error("{0:?} is not of the form cas/b/<build>/<nonce>")]
    NotAPackKey(String),
    /// The nonce is not 40 lowercase hex digits followed by `-` and a slot.
    #[error("the nonce {0:?} is not <32 hex MAC><8 hex minute>-<slot>")]
    Nonce(String),
    /// The slot is not a decimal number without sign or leading zeros.
    #[error("the slot {0:?} is not a canonical decimal u32")]
    Slot(String),
    /// A time that does not fit the 32-bit minute of a key.
    #[error("{0} seconds after the epoch is past the last minute a key can name")]
    MinuteRange(u64),
}

/// A parsed task pack key.
///
/// The text form ([`fmt::Display`] and [`TaskPackKey::parse`]) is canonical: lowercase hex, a
/// slot without leading zeros, so one key has one spelling.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskPackKey {
    build: String,
    mac: [u8; MAC_LEN],
    minute: u32,
    slot: u32,
}

impl TaskPackKey {
    /// A key for `build` with the given MAC, mint minute (minutes since the Unix epoch) and slot.
    ///
    /// # Errors
    /// The refusal of [`check_build`].
    pub fn new(
        build: &str,
        mac: [u8; MAC_LEN],
        minute: u32,
        slot: u32,
    ) -> Result<Self, TaskKeyError> {
        check_build(build)?;
        Ok(Self {
            build: build.to_owned(),
            mac,
            minute,
            slot,
        })
    }

    /// Parses `cas/b/<build>/<nonce>-<slot>`.
    ///
    /// # Errors
    /// [`TaskKeyError::NotAPackKey`] for another shape (another prefix, a missing or extra
    /// `/`), the refusal of [`check_build`] for the build, [`TaskKeyError::Nonce`] for a nonce
    /// that is not 40 lowercase hex digits then `-`, [`TaskKeyError::Slot`] for a slot that is
    /// not a canonical decimal.
    pub fn parse(key: &str) -> Result<Self, TaskKeyError> {
        let not_a_key = || TaskKeyError::NotAPackKey(key.to_owned());
        let rest = key.strip_prefix(PREFIX).ok_or_else(not_a_key)?;
        let (build, nonce) = rest.split_once('/').ok_or_else(not_a_key)?;
        if nonce.contains('/') {
            return Err(not_a_key());
        }
        check_build(build)?;
        let bad_nonce = || TaskKeyError::Nonce(nonce.to_owned());
        let (digits, slot) = nonce.split_once('-').ok_or_else(bad_nonce)?;
        if digits.len() != 2 * MAC_LEN + 8 || !is_lower_hex(digits) {
            return Err(bad_nonce());
        }
        let (mac_hex, minute_hex) = digits.split_at(2 * MAC_LEN);
        // Both halves are lowercase hex of the right length (checked above), so these cannot fail.
        let mac = std::array::from_fn(|i| {
            u8::from_str_radix(&mac_hex[2 * i..2 * i + 2], 16).unwrap_or_default()
        });
        let minute = u32::from_str_radix(minute_hex, 16).unwrap_or_default();
        Self::new(build, mac, minute, parse_slot(slot)?)
    }

    /// The build id: the lease the frontend issued.
    #[must_use]
    pub fn build(&self) -> &str {
        &self.build
    }

    /// The MAC the nonce carries.
    #[must_use]
    pub const fn mac(&self) -> &[u8; MAC_LEN] {
        &self.mac
    }

    /// The minute the key was minted, in minutes since the Unix epoch.
    #[must_use]
    pub const fn minute(&self) -> u32 {
        self.minute
    }

    /// The start of the mint minute, in seconds since the Unix epoch.
    #[must_use]
    pub const fn minted_unix_secs(&self) -> u64 {
        self.minute as u64 * 60
    }

    /// The slot: which of a lease's packs this is (always 0 in v1).
    #[must_use]
    pub const fn slot(&self) -> u32 {
        self.slot
    }
}

impl fmt::Display for TaskPackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PREFIX}{}/{}{:08x}-{}",
            self.build,
            hex::encode(self.mac),
            self.minute,
            self.slot
        )
    }
}

/// Whether `build` may be the middle part of a key: 1 to [`MAX_BUILD_BYTES`] characters of
/// `0-9 a-z A-Z - _`. The frontend's lease ids are 32 hex digits; the rule is looser only
/// so that it does not need the lease's own format, and it holds no `/`, so a build cannot
/// move a key out of its prefix.
///
/// # Errors
/// [`TaskKeyError::EmptyBuild`], [`TaskKeyError::LongBuild`] or [`TaskKeyError::BuildCharacter`].
pub fn check_build(build: &str) -> Result<(), TaskKeyError> {
    if build.is_empty() {
        return Err(TaskKeyError::EmptyBuild);
    }
    if build.len() > MAX_BUILD_BYTES {
        return Err(TaskKeyError::LongBuild(build.len()));
    }
    match build
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(*c, '-' | '_')))
    {
        Some(c) => Err(TaskKeyError::BuildCharacter(c)),
        None => Ok(()),
    }
}

/// The minute (minutes since the Unix epoch) that `unix_secs` falls in.
///
/// # Errors
/// [`TaskKeyError::MinuteRange`] past the year 10000.
pub fn minute_of(unix_secs: u64) -> Result<u32, TaskKeyError> {
    u32::try_from(unix_secs / 60).map_err(|_| TaskKeyError::MinuteRange(unix_secs))
}

/// The message the MAC of a key covers: the domain tag `kbf-task-pack/v1`, a NUL, the build,
/// a NUL, the action's 32 hash bytes and its size (big-endian u64), then the mint minute
/// (big-endian u32). A build holds no NUL and the rest is fixed width, so two different
/// (build, action, minute) triples never give the same message.
///
/// # Errors
/// The refusal of [`check_build`].
pub fn mac_input(build: &str, action: &Digest, minute: u32) -> Result<Vec<u8>, TaskKeyError> {
    check_build(build)?;
    let mut out = Vec::with_capacity(MAC_DOMAIN.len() + build.len() + 2 + 32 + 8 + 4);
    out.extend_from_slice(MAC_DOMAIN);
    out.push(0);
    out.extend_from_slice(build.as_bytes());
    out.push(0);
    out.extend_from_slice(&action.hash);
    out.extend_from_slice(&action.size_bytes.to_be_bytes());
    out.extend_from_slice(&minute.to_be_bytes());
    Ok(out)
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn parse_slot(slot: &str) -> Result<u32, TaskKeyError> {
    let bad = || TaskKeyError::Slot(slot.to_owned());
    let digits = !slot.is_empty() && slot.bytes().all(|b| b.is_ascii_digit());
    let leading_zero = slot.len() > 1 && slot.starts_with('0');
    if !digits || leading_zero {
        return Err(bad());
    }
    slot.parse().map_err(|_| bad())
}
