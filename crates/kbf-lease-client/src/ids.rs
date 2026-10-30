//! The two ids that appear in a request path. Both are checked when they are made, so a
//! path built from one can never carry a slash, a dot segment or a query.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A lease id, `<term>-<sequence>`: the server's boot count and a running number. The
/// server reads an id only in the form it prints, so `01-2` names nothing; this type
/// reads the same form and no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeaseId {
    term: u64,
    sequence: u64,
}

impl LeaseId {
    /// The id of lease `sequence` of boot `term`.
    #[must_use]
    pub fn new(term: u64, sequence: u64) -> Self {
        Self { term, sequence }
    }

    /// The server's boot count when the lease was granted.
    #[must_use]
    pub fn term(self) -> u64 {
        self.term
    }

    /// The lease's running number.
    #[must_use]
    pub fn sequence(self) -> u64 {
        self.sequence
    }
}

impl fmt::Display for LeaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.term, self.sequence)
    }
}

/// The text is not a lease id in the form the server prints.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a lease id (<term>-<sequence>, both canonical decimal numbers)")]
pub struct BadLeaseId(String);

impl FromStr for LeaseId {
    type Err = BadLeaseId;

    fn from_str(text: &str) -> Result<Self, BadLeaseId> {
        let bad = || BadLeaseId(text.to_owned());
        let (term, sequence) = text.split_once('-').ok_or_else(bad)?;
        let id = Self::new(
            term.parse().map_err(|_| bad())?,
            sequence.parse().map_err(|_| bad())?,
        );
        // `+1`, `01` and the like parse as numbers but are not what the server prints.
        (id.to_string() == text).then_some(id).ok_or_else(bad)
    }
}

impl Serialize for LeaseId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LeaseId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// A run id, `run-<term>-<number>` as the server mints them. The server's contract is only
/// that it is a path segment, so this type holds what is safe in one: 1 to 128 bytes of
/// `[A-Za-z0-9._-]`, and not `.` or `..`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(String);

impl RunId {
    /// The longest run id, in bytes.
    pub const MAX_LEN: usize = 128;

    /// The id the server mints for run `number` of boot `term`.
    #[cfg(feature = "fake")]
    pub(crate) fn minted(term: u64, number: u64) -> Self {
        Self(format!("run-{term}-{number}"))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The text cannot be a path segment naming a run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a run id (1 to 128 bytes of [A-Za-z0-9._-], not . or ..)")]
pub struct BadRunId(String);

impl FromStr for RunId {
    type Err = BadRunId;

    fn from_str(text: &str) -> Result<Self, BadRunId> {
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
        let fits = !text.is_empty()
            && text.len() <= Self::MAX_LEN
            && text.bytes().all(plain)
            && text != "."
            && text != "..";
        fits.then(|| Self(text.to_owned()))
            .ok_or_else(|| BadRunId(text.to_owned()))
    }
}

impl Serialize for RunId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
