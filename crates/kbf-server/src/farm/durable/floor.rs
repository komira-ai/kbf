//! The key and value of a `floor/<instance>/<action>` row: one memory floor
//! ([`kbf_sched::Floor`]), written with the requeue that raises it.
//!
//! The row's key is `floor/`, the instance name in hexadecimal (the name may hold any
//! character, a `/` too), `/`, and the action's digest as `<hash>/<size>`: one key per
//! action key, and no two alike. The value is a version byte (1), the ask (8 bytes,
//! big-endian), the stamp (8), the instance name (2-byte length, then the bytes) and the
//! digest's text to the end. Every digest is SHA-256 (the only [`DigestFunction`]). A row
//! of another version is refused, not guessed at.

use kbf_sched::{Floor, KeyFloor};
use kbf_types::{ActionKey, Digest, DigestFunction, WorkKey};

const VERSION: u8 = 1;

/// The key of `key`'s row: the key's class is the instance name and its dedup text the
/// action's digest as `<hash>/<size>`.
pub(super) fn row_key(key: &WorkKey) -> String {
    let instance: String = key
        .class
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{}{instance}/{}", super::FLOOR, key.dedup)
}

/// `floor` as a row value, or `None` if its instance name is too long for it.
pub(super) fn encode(floor: &Floor) -> Option<Vec<u8>> {
    let instance = floor.key.class.as_bytes();
    let mut row = vec![VERSION];
    row.extend(floor.ask.to_be_bytes());
    row.extend(floor.stamp.to_be_bytes());
    row.extend(u16::try_from(instance.len()).ok()?.to_be_bytes());
    row.extend(instance);
    row.extend(floor.key.dedup.as_bytes());
    Some(row)
}

/// A row value as a [`Floor`], or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<Floor, &'static str> {
    let (version, rest) = row.split_first().ok_or("the row ends early")?;
    if *version != VERSION {
        return Err("not a version 1 floor");
    }
    let (ask, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (stamp, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (len, rest) = rest.split_first_chunk::<2>().ok_or("the row ends early")?;
    let len = usize::from(u16::from_be_bytes(*len));
    let (instance, digest) = rest.split_at_checked(len).ok_or("the row ends early")?;
    let instance = std::str::from_utf8(instance).map_err(|_| "an instance name is not text")?;
    let digest = std::str::from_utf8(digest).map_err(|_| "a digest is not text")?;
    let action = Digest::parse(DigestFunction::Sha256, digest).map_err(|_| "not a digest")?;
    Ok(Floor {
        key: ActionKey {
            instance: instance.to_owned(),
            action,
        }
        .into(),
        ask: u64::from_be_bytes(*ask),
        stamp: u64::from_be_bytes(*stamp),
    })
}

/// The key of the `keyfloor/` row of size key `key`: 16 hexadecimal digits.
pub(super) fn key_row(key: u64) -> String {
    format!("{}{key:016x}", super::KEY_FLOOR)
}

/// `floor` as a row value: a version byte (1), the key, the ask and the stamp (8 bytes
/// each, big-endian), then the name of the size summary the key's floor decays with (the
/// key of its `est/` row, behind a two-byte length; empty for none).
pub(super) fn encode_key(floor: &KeyFloor, summary: Option<&str>) -> Vec<u8> {
    let name = summary.unwrap_or("").as_bytes();
    let mut row = vec![VERSION];
    row.extend(floor.key.to_be_bytes());
    row.extend(floor.ask.to_be_bytes());
    row.extend(floor.stamp.to_be_bytes());
    row.extend(u16::try_from(name.len()).unwrap_or(0).to_be_bytes());
    row.extend(name);
    row
}

/// A `keyfloor/` row value as a [`KeyFloor`] and the name of its size summary, or what is
/// wrong with it.
pub(super) fn decode_key(row: &[u8]) -> Result<(KeyFloor, Option<String>), &'static str> {
    let (version, rest) = row.split_first().ok_or("the row ends early")?;
    if *version != VERSION {
        return Err("not a version 1 key floor");
    }
    let (key, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (ask, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (stamp, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (len, rest) = rest.split_first_chunk::<2>().ok_or("the row ends early")?;
    if rest.len() != usize::from(u16::from_be_bytes(*len)) {
        return Err("the row is not the length it says");
    }
    let name = std::str::from_utf8(rest).map_err(|_| "a summary name is not text")?;
    let floor = KeyFloor {
        key: u64::from_be_bytes(*key),
        ask: u64::from_be_bytes(*ask),
        stamp: u64::from_be_bytes(*stamp),
    };
    Ok((floor, (!name.is_empty()).then(|| name.to_owned())))
}
