//! The key and value of an `est/...` row: one size summary of the learned-sizes shadow
//! ([`crate::learned`]).
//!
//! The row's key is `est/`, the instance name and the platform class in hexadecimal (they
//! may hold any character, a `/` too), the basis (`c` for cgroup, `m` for macOS footprint),
//! the shape as sixteen hex digits and the configuration as sixteen hex digits or `-`,
//! joined by `/`: one key per estimator key, and no two alike. The value is a version
//! byte (1) and the estimator's own row (versioned and checksummed by `kbf-estimator`).
//! A row of another version, or one the key of which does not parse, is skipped by a
//! reader, never guessed at: the action it names starts cold.

use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};

const VERSION: u8 = 1;

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<String> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

/// The key of `key`'s row.
pub(crate) fn row_key(key: &Key) -> String {
    let basis = match key.basis() {
        Basis::Cgroup => 'c',
        Basis::MacFootprint => 'm',
    };
    let cfg = key
        .cfg()
        .map_or_else(|| "-".to_owned(), |Cfg(cfg)| format!("{cfg:016x}"));
    format!(
        "{}{}/{}/{basis}/{:016x}/{cfg}",
        super::ESTIMATE,
        hex(key.instance().as_str()),
        hex(key.platform().as_str()),
        key.shape().0,
    )
}

/// The value of a row holding the estimator's `summary`.
pub(crate) fn encode(summary: &[u8]) -> Vec<u8> {
    [&[VERSION][..], summary].concat()
}

/// The key a row key names, or `None` for one that does not parse or is not spelled the
/// way [`row_key`] spells it (two rows could name one key).
pub(crate) fn parse_key(row_key: &str) -> Option<Key> {
    let mut parts = row_key.strip_prefix(super::ESTIMATE)?.split('/');
    let instance = unhex(parts.next()?)?;
    let platform = unhex(parts.next()?)?;
    let basis = match parts.next()? {
        "c" => Basis::Cgroup,
        "m" => Basis::MacFootprint,
        _ => return None,
    };
    let shape = u64::from_str_radix(parts.next()?, 16).ok()?;
    let cfg = match parts.next()? {
        "-" => None,
        text => Some(Cfg(u64::from_str_radix(text, 16).ok()?)),
    };
    if parts.next().is_some() {
        return None;
    }
    let key = Key::new(
        Instance::new(instance),
        PlatformClass::new(platform),
        basis,
        Shape(shape),
        cfg,
    );
    (row_key == self::row_key(&key)).then_some(key)
}

/// The key a row key names, and the estimator's row its value holds.
pub(crate) fn decode(row_key: &str, value: &[u8]) -> Option<(Key, Vec<u8>)> {
    let (version, summary) = value.split_first()?;
    if *version != VERSION {
        return None;
    }
    parse_key(row_key).map(|key| (key, summary.to_vec()))
}
