//! The record of a run, and the hash that identifies it.

use std::fmt;

use sha2::{Digest, Sha256};

/// The SHA-256 of a run's trace lines. Two runs with equal hashes made the same
/// decisions in the same order; a seed that replays to a different hash has found
/// nondeterminism.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceHash([u8; 32]);

impl TraceHash {
    /// The raw hash bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for TraceHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for TraceHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceHash({self})")
    }
}

/// Every event of a run, folded into a hash as it happens and optionally kept as text.
#[derive(Clone, Debug)]
pub(crate) struct Trace {
    hasher: Sha256,
    lines: Option<Vec<String>>,
}

impl Trace {
    pub(crate) fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            lines: None,
        }
    }

    pub(crate) fn keep_lines(&mut self) {
        self.lines.get_or_insert_with(Vec::new);
    }

    pub(crate) fn record(&mut self, line: String) {
        self.hasher.update(line.as_bytes());
        self.hasher.update(b"\n");
        if let Some(lines) = &mut self.lines {
            lines.push(line);
        }
    }

    pub(crate) fn hash(&self) -> TraceHash {
        TraceHash(self.hasher.clone().finalize().into())
    }

    pub(crate) fn lines(&self) -> Option<&[String]> {
        self.lines.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a hash that ignores line order or line boundaries ("ab" + "c" hashing
    /// like "a" + "bc"), either of which would let two different runs share a hash.
    #[test]
    fn hash_sees_order_and_boundaries() {
        let hash = |lines: &[&str]| {
            let mut t = Trace::new();
            lines.iter().for_each(|l| t.record((*l).to_owned()));
            t.hash()
        };
        assert_eq!(hash(&["a", "b"]), hash(&["a", "b"]));
        assert_ne!(hash(&["a", "b"]), hash(&["b", "a"]));
        assert_ne!(hash(&["ab", "c"]), hash(&["a", "bc"]));
        assert_eq!(
            hash(&[]).to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Catches: `as_bytes` and the `Debug` form disagreeing with the `Display` hex that
    /// run reports quote (a failing seed is replayed by comparing these strings).
    #[test]
    fn a_hash_shows_the_same_bytes_in_every_form() {
        let mut t = Trace::new();
        t.record("x".to_owned());
        let h = t.hash();
        let hex: String = h.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(h.to_string(), hex);
        assert_eq!(format!("{h:?}"), format!("TraceHash({hex})"));
        assert_eq!(hex.len(), 64);
    }

    /// Catches: lines kept when nobody asked for them, or `keep_lines` dropping lines
    /// recorded before it and failing to keep the ones after.
    #[test]
    fn lines_are_kept_only_once_asked_for() {
        let mut t = Trace::new();
        t.record("before".to_owned());
        assert_eq!(t.lines(), None);
        t.keep_lines();
        t.record("after".to_owned());
        assert_eq!(t.lines(), Some(&["after".to_owned()][..]));
    }
}
