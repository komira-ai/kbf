//! The key of a task pack or large object, in one of two layouts ([`KeyLayout`]):
//!
//! - **Slotted**, `cas/b/<build>/<nonce>-<slot>`, the slot being `<k>` for pack `k` or `L<i>`
//!   for large object `i` ([`Slot`]): one key per object, each with a presigned write. The
//!   nonce is 32 hex digits of a MAC (the frontend's HMAC over the lease, the action, the mint
//!   minute and the slot, truncated to 16 bytes, [`mac_input`]) followed by the mint minute in
//!   8 hex digits.
//! - **Put directory**, `cas/b/<build>/<nonce>/<name>`, the name being `pack` for pack 0,
//!   `pack<k>` for pack `k` and `L<i>` for large object `i` ([`Slot::object_name`]): the task
//!   writes files into the put directory `cas/b/<build>/<nonce>/` and each committed file is the
//!   object of that key. The nonce is the same 40 hex digits, but its MAC covers no slot
//!   ([`mac_input_put_dir`]): one MAC names the attempt's directory, and the name in it says the
//!   slot.
//!
//! The task never picks a key or a directory. The janitor reads an object's age from the
//! minute in its key alone, in either layout. This module is the shape of a key and the message
//! the MAC covers; the secret, the HMAC and the comparison against a presented key belong to
//! the frontend that mints.

use std::fmt;

use kbf_types::Digest;

/// The prefix every task pack key starts with.
const PREFIX: &str = "cas/b/";

/// The domain tag at the start of the MAC's message.
const MAC_DOMAIN: &[u8] = b"kbf-task-pack/v2";

/// The domain tag of the first MAC, which did not cover the slot: only slot 0 was ever minted
/// under it, so it verifies for [`Slot::Pack`]`(0)` and nothing else.
const MAC_DOMAIN_V1: &[u8] = b"kbf-task-pack/v1";

/// The domain tag of the MAC of a put directory, which covers no slot. A tag of its own, so a
/// put directory's nonce is never one a slotted key of slot 0 carries (`kbf-task-pack/v1`).
const MAC_DOMAIN_PUT_DIR: &[u8] = b"kbf-task-pack/put-dir/v1";

/// The name of pack 0 in a put directory.
const PACK_NAME: &str = "pack";

/// The most packs one run may write: slots `0..MAX_PACKS`.
pub const MAX_PACKS: u8 = 8;

/// The most large objects one run may write: slots `L0..L(MAX_LARGE-1)`.
pub const MAX_LARGE: u8 = 8;

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
    /// The nonce is not 40 lowercase hex digits followed by `-` and a slot (slotted), or not 40
    /// lowercase hex digits (put directory).
    #[error("the nonce {0:?} is not <32 hex MAC><8 hex minute>, then -<slot> or /<name>")]
    Nonce(String),
    /// The slot is not `<k>` or `L<i>` with a canonical decimal (no sign, no leading zeros) below
    /// [`MAX_PACKS`] or [`MAX_LARGE`].
    #[error("the slot {0:?} is not <k> (k < {MAX_PACKS}) or L<i> (i < {MAX_LARGE})")]
    Slot(String),
    /// The object name in a put directory is not `pack`, `pack<k>` (`0 < k <` [`MAX_PACKS`]) or
    /// `L<i>` (`i <` [`MAX_LARGE`]), each index a canonical decimal.
    #[error("the name {0:?} is not pack, pack<k> (0 < k < {MAX_PACKS}) or L<i> (i < {MAX_LARGE})")]
    Name(String),
    /// A [`Slot`] value outside the slots a run has.
    #[error("the slot {0} is outside the {MAX_PACKS} pack and {MAX_LARGE} large slots of a run")]
    SlotRange(Slot),
    /// A time that does not fit the 32-bit minute of a key.
    #[error("{0} seconds after the epoch is past the last minute a key can name")]
    MinuteRange(u64),
}

/// Which object of a run a key is for: pack `k` (`<k>`, `k < MAX_PACKS`) or large object `i`
/// (`L<i>`, `i < MAX_LARGE`). Text forms are canonical decimals: no sign, no leading zeros.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Slot {
    /// A kbf-segments pack; pack 0 holds the manifest.
    Pack(u8),
    /// A raw object holding one blob over the record cap.
    Large(u8),
}

impl Slot {
    /// Whether the index is below [`MAX_PACKS`] or [`MAX_LARGE`].
    #[must_use]
    pub const fn in_range(self) -> bool {
        match self {
            Self::Pack(k) => k < MAX_PACKS,
            Self::Large(i) => i < MAX_LARGE,
        }
    }

    /// Parses `<k>` or `L<i>`.
    ///
    /// # Errors
    /// [`TaskKeyError::Slot`] for any other text, a sign, a leading zero or an index out of range.
    pub fn parse(text: &str) -> Result<Self, TaskKeyError> {
        let bad = || TaskKeyError::Slot(text.to_owned());
        let (large, digits) = match text.strip_prefix('L') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let canonical = !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && !(digits.len() > 1 && digits.starts_with('0'));
        let index: u8 = if canonical {
            digits.parse().map_err(|_| bad())?
        } else {
            return Err(bad());
        };
        let slot = if large {
            Self::Large(index)
        } else {
            Self::Pack(index)
        };
        if slot.in_range() {
            Ok(slot)
        } else {
            Err(bad())
        }
    }

    /// The name of this slot's object in a put directory: `pack` for pack 0, `pack<k>` for pack
    /// `k` and `L<i>` for large object `i`, the names `kbf-task` commits.
    #[must_use]
    pub fn object_name(self) -> String {
        match self {
            Self::Pack(0) => PACK_NAME.to_owned(),
            Self::Pack(k) => format!("{PACK_NAME}{k}"),
            Self::Large(i) => format!("L{i}"),
        }
    }

    /// Parses an object name of a put directory, the inverse of [`Slot::object_name`].
    ///
    /// # Errors
    /// [`TaskKeyError::Name`] for any other text: `pack0`, a sign, a leading zero, an index out
    /// of range, a bare `<k>`.
    pub fn from_object_name(name: &str) -> Result<Self, TaskKeyError> {
        let bad = || TaskKeyError::Name(name.to_owned());
        if name == PACK_NAME {
            return Ok(Self::Pack(0));
        }
        let slot = if let Some(index) = name.strip_prefix(PACK_NAME) {
            match Self::parse(index).map_err(|_| bad())? {
                // `pack0` would be a second spelling of `pack`.
                Self::Pack(0) | Self::Large(_) => return Err(bad()),
                pack => pack,
            }
        } else if name.starts_with('L') {
            Self::parse(name).map_err(|_| bad())?
        } else {
            return Err(bad());
        };
        Ok(slot)
    }

    /// The bytes the v2 MAC covers for this slot: a kind byte (0 pack, 1 large) and the index.
    const fn mac_bytes(self) -> [u8; 2] {
        match self {
            Self::Pack(k) => [0, k],
            Self::Large(i) => [1, i],
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pack(k) => write!(f, "{k}"),
            Self::Large(i) => write!(f, "L{i}"),
        }
    }
}

/// How a key spells its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyLayout {
    /// `cas/b/<build>/<nonce>-<slot>`: a key per object, its MAC over the slot.
    Slotted,
    /// `cas/b/<build>/<nonce>/<name>`: an object of a put directory, its MAC over the directory.
    PutDir,
}

/// A parsed task pack key.
///
/// The text form ([`fmt::Display`] and [`TaskPackKey::parse`]) is canonical: lowercase hex, a
/// slot without leading zeros, so one key has one spelling in each layout.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaskPackKey {
    build: String,
    mac: [u8; MAC_LEN],
    minute: u32,
    slot: Slot,
    layout: KeyLayout,
}

impl TaskPackKey {
    /// A slotted key for `build` with the given MAC, mint minute (minutes since the Unix epoch)
    /// and slot.
    ///
    /// # Errors
    /// The refusal of [`check_build`], and [`TaskKeyError::SlotRange`] for a slot a run does not
    /// have.
    pub fn new(
        build: &str,
        mac: [u8; MAC_LEN],
        minute: u32,
        slot: Slot,
    ) -> Result<Self, TaskKeyError> {
        Self::laid_out(build, mac, minute, slot, KeyLayout::Slotted)
    }

    /// The key of the object `slot` in the put directory of `build` whose nonce is `mac` and
    /// `minute`: `cas/b/<build>/<nonce>/<name>`.
    ///
    /// # Errors
    /// As [`TaskPackKey::new`].
    pub fn in_put_dir(
        build: &str,
        mac: [u8; MAC_LEN],
        minute: u32,
        slot: Slot,
    ) -> Result<Self, TaskKeyError> {
        Self::laid_out(build, mac, minute, slot, KeyLayout::PutDir)
    }

    fn laid_out(
        build: &str,
        mac: [u8; MAC_LEN],
        minute: u32,
        slot: Slot,
        layout: KeyLayout,
    ) -> Result<Self, TaskKeyError> {
        check_build(build)?;
        if !slot.in_range() {
            return Err(TaskKeyError::SlotRange(slot));
        }
        Ok(Self {
            build: build.to_owned(),
            mac,
            minute,
            slot,
            layout,
        })
    }

    /// Parses `cas/b/<build>/<nonce>-<slot>` or `cas/b/<build>/<nonce>/<name>`.
    ///
    /// # Errors
    /// [`TaskKeyError::NotAPackKey`] for another shape (another prefix, a missing `/`, or more
    /// than one after the nonce), the refusal of [`check_build`] for the build,
    /// [`TaskKeyError::Nonce`] for a nonce that is not 40 lowercase hex digits (then `-` in the
    /// slotted layout), [`TaskKeyError::Slot`] for a slot that is not `<k>` or `L<i>` in
    /// canonical form and in range, [`TaskKeyError::Name`] for a name that is not one of
    /// [`Slot::object_name`]'s.
    pub fn parse(key: &str) -> Result<Self, TaskKeyError> {
        let not_a_key = || TaskKeyError::NotAPackKey(key.to_owned());
        let rest = key.strip_prefix(PREFIX).ok_or_else(not_a_key)?;
        let (build, nonce) = rest.split_once('/').ok_or_else(not_a_key)?;
        let (nonce, name) = match nonce.split_once('/') {
            Some((_, name)) if name.contains('/') => return Err(not_a_key()),
            Some((nonce, name)) => (nonce, Some(name)),
            None => (nonce, None),
        };
        check_build(build)?;
        let bad_nonce = || TaskKeyError::Nonce(nonce.to_owned());
        let (digits, slot, layout) = match name {
            Some(name) => (nonce, Slot::from_object_name(name), KeyLayout::PutDir),
            None => {
                let (digits, slot) = nonce.split_once('-').ok_or_else(bad_nonce)?;
                (digits, Slot::parse(slot), KeyLayout::Slotted)
            }
        };
        let (mac, minute) = nonce_parts(digits).ok_or_else(bad_nonce)?;
        Self::laid_out(build, mac, minute, slot?, layout)
    }

    /// How the key spells its slot.
    #[must_use]
    pub const fn layout(&self) -> KeyLayout {
        self.layout
    }

    /// The put directory the key is an object of, `cas/b/<build>/<nonce>` (no `/` at its
    /// end); `None` for a slotted key.
    #[must_use]
    pub fn put_dir(&self) -> Option<String> {
        match self.layout {
            KeyLayout::PutDir => Some(format!(
                "{PREFIX}{}/{}{:08x}",
                self.build,
                hex::encode(self.mac),
                self.minute
            )),
            KeyLayout::Slotted => None,
        }
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

    /// The slot: which object of a run's key set this is.
    #[must_use]
    pub const fn slot(&self) -> Slot {
        self.slot
    }
}

impl fmt::Display for TaskPackKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (build, mac, minute) = (&self.build, hex::encode(self.mac), self.minute);
        match self.layout {
            KeyLayout::Slotted => write!(f, "{PREFIX}{build}/{mac}{minute:08x}-{}", self.slot),
            KeyLayout::PutDir => write!(
                f,
                "{PREFIX}{build}/{mac}{minute:08x}/{}",
                self.slot.object_name()
            ),
        }
    }
}

/// The MAC and the minute of a nonce's 40 lowercase hex digits; `None` for any other text.
fn nonce_parts(digits: &str) -> Option<([u8; MAC_LEN], u32)> {
    if digits.len() != 2 * MAC_LEN + 8 || !is_lower_hex(digits) {
        return None;
    }
    let (mac_hex, minute_hex) = digits.split_at(2 * MAC_LEN);
    // Both halves are lowercase hex of the right length (checked above), so these cannot fail.
    let mac = std::array::from_fn(|i| {
        u8::from_str_radix(&mac_hex[2 * i..2 * i + 2], 16).unwrap_or_default()
    });
    let minute = u32::from_str_radix(minute_hex, 16).unwrap_or_default();
    Some((mac, minute))
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

/// The message the MAC of a key covers: the domain tag `kbf-task-pack/v2`, a NUL, the build,
/// a NUL, the action's 32 hash bytes and its size (big-endian u64), the mint minute
/// (big-endian u32), then the slot (a kind byte, 0 for a pack and 1 for a large object, and the
/// index). A build holds no NUL and the rest is fixed width, so two different
/// (build, action, minute, slot) tuples never give the same message.
///
/// # Errors
/// The refusal of [`check_build`], and [`TaskKeyError::SlotRange`] for a slot a run does not have.
pub fn mac_input(
    build: &str,
    action: &Digest,
    minute: u32,
    slot: Slot,
) -> Result<Vec<u8>, TaskKeyError> {
    if !slot.in_range() {
        return Err(TaskKeyError::SlotRange(slot));
    }
    let mut out = message(MAC_DOMAIN, build, action, minute)?;
    out.extend_from_slice(&slot.mac_bytes());
    Ok(out)
}

/// The message of the first MAC, `kbf-task-pack/v1`: [`mac_input`] without the slot. Keys minted
/// under it are all slot 0, so a verifier accepts it for [`Slot::Pack`]`(0)` only; nothing mints
/// under it any more.
///
/// # Errors
/// The refusal of [`check_build`].
pub fn mac_input_v1(build: &str, action: &Digest, minute: u32) -> Result<Vec<u8>, TaskKeyError> {
    message(MAC_DOMAIN_V1, build, action, minute)
}

/// The message the MAC of a put directory's nonce covers: [`mac_input_v1`]'s fields under the
/// domain tag `kbf-task-pack/put-dir/v1`, and no slot. Every object of the directory carries
/// the same nonce; the name after it says which object it is, and the directory is the
/// attempt's alone.
///
/// # Errors
/// The refusal of [`check_build`].
pub fn mac_input_put_dir(
    build: &str,
    action: &Digest,
    minute: u32,
) -> Result<Vec<u8>, TaskKeyError> {
    message(MAC_DOMAIN_PUT_DIR, build, action, minute)
}

fn message(
    domain: &[u8],
    build: &str,
    action: &Digest,
    minute: u32,
) -> Result<Vec<u8>, TaskKeyError> {
    check_build(build)?;
    let mut out = Vec::with_capacity(domain.len() + build.len() + 2 + 32 + 8 + 4 + 2);
    out.extend_from_slice(domain);
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
