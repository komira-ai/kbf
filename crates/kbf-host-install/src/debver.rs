//! Debian version ordering, as `dpkg --compare-versions` does it (deb-version(7)).
//!
//! A version is `[epoch:]upstream[-revision]`. The epoch is compared as a number; the
//! upstream version and the revision are compared by [`verrevcmp`]: alternating runs of
//! non-digits (compared character by character, `~` before the end of the text, the end
//! before a letter, a letter before any other character) and digits (compared as numbers).

use std::cmp::Ordering;

/// A version that does not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DebVersionError {
    #[error("empty version")]
    Empty,
    #[error("a character outside the Debian version alphabet")]
    BadChar,
    #[error("the epoch is not a number")]
    BadEpoch,
    #[error("the upstream version is empty")]
    EmptyUpstream,
    #[error("the upstream version does not start with a digit")]
    UpstreamNotDigit,
    #[error("the revision is empty")]
    EmptyRevision,
}

/// A parsed Debian version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebVersion {
    epoch: u64,
    upstream: String,
    revision: String,
}

impl DebVersion {
    /// Parse `text`.
    ///
    /// # Errors
    /// [`DebVersionError`] for text dpkg would refuse as a version.
    pub fn parse(_text: &str) -> Result<Self, DebVersionError> {
        todo!()
    }
}

impl Ord for DebVersion {
    fn cmp(&self, _other: &Self) -> Ordering {
        todo!()
    }
}

impl PartialOrd for DebVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests;
