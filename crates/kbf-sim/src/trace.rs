//! The record of a run, and the hash that identifies it.

use std::fmt;

use sha2::{Digest, Sha256};

/// The version of the trace text format, written as the first line of every trace.
///
/// A trace line is a hand-written format (see [`crate::text`]); this number changes
/// when any of those formats does, which changes every golden hash (see
/// [`crate::golden`]).
pub const FORMAT_VERSION: u32 = 1;

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

/// Every line of a run, folded into a hash as it happens and optionally kept as text.
///
/// The first line is always `kbf-trace-format <FORMAT_VERSION> <family>`, where the
/// family names the grammar of the lines after it (the kernel's own lines, or a
/// scenario world's). It is hashed like any other line, so a hash identifies the format
/// as well as the run.
#[derive(Clone, Debug)]
pub struct Trace {
    hasher: Sha256,
    header: String,
    lines: Option<Vec<String>>,
}

impl Trace {
    /// An empty trace of `family`, holding only its format line.
    #[must_use]
    pub fn new(family: &str) -> Self {
        let header = format!("kbf-trace-format {FORMAT_VERSION} {family}");
        let mut trace = Self {
            hasher: Sha256::new(),
            header: header.clone(),
            lines: None,
        };
        trace.hash_line(&header);
        trace
    }

    /// Keeps the lines as text from now on (the format line, and every line recorded
    /// after this call), for [`Trace::lines`].
    pub fn keep_lines(&mut self) {
        let header = &self.header;
        self.lines.get_or_insert_with(|| vec![header.clone()]);
    }

    /// Appends one line. The line holds no newline.
    pub fn record(&mut self, line: &str) {
        self.hash_line(line);
        if let Some(lines) = &mut self.lines {
            lines.push(line.to_owned());
        }
    }

    /// The hash of every line so far, newline after each.
    #[must_use]
    pub fn hash(&self) -> TraceHash {
        TraceHash(self.hasher.clone().finalize().into())
    }

    /// The kept lines, if [`Trace::keep_lines`] was called.
    #[must_use]
    pub fn lines(&self) -> Option<&[String]> {
        self.lines.as_deref()
    }

    fn hash_line(&mut self, line: &str) {
        self.hasher.update(line.as_bytes());
        self.hasher.update(b"\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(family: &str, lines: &[&str]) -> TraceHash {
        let mut t = Trace::new(family);
        lines.iter().for_each(|l| t.record(l));
        t.hash()
    }

    /// Catches: a hash that ignores line order or line boundaries ("ab" + "c" hashing
    /// like "a" + "bc"), either of which would let two different runs share a hash.
    #[test]
    fn hash_sees_order_and_boundaries() {
        assert_eq!(hash_of("t", &["a", "b"]), hash_of("t", &["a", "b"]));
        assert_ne!(hash_of("t", &["a", "b"]), hash_of("t", &["b", "a"]));
        assert_ne!(hash_of("t", &["ab", "c"]), hash_of("t", &["a", "bc"]));
    }

    /// Catches: a format line that is not hashed (two format versions or two families
    /// would share hashes), or whose bytes change (every golden hash would move).
    #[test]
    fn the_format_line_is_first_and_hashed() {
        let mut t = Trace::new("sim");
        t.keep_lines();
        assert_eq!(t.lines(), Some(&["kbf-trace-format 1 sim".to_owned()][..]));
        let expected: [u8; 32] = Sha256::digest(b"kbf-trace-format 1 sim\n").into();
        assert_eq!(t.hash().as_bytes(), &expected);
        assert_ne!(hash_of("sim", &[]), hash_of("other", &[]));
    }

    /// Catches: `as_bytes` and the `Debug` form disagreeing with the `Display` hex that
    /// run reports quote (a failing seed is replayed by comparing these strings).
    #[test]
    fn a_hash_shows_the_same_bytes_in_every_form() {
        let h = hash_of("t", &["x"]);
        let hex: String = h.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(h.to_string(), hex);
        assert_eq!(format!("{h:?}"), format!("TraceHash({hex})"));
        assert_eq!(hex.len(), 64);
    }

    /// Catches: lines kept when nobody asked for them, or `keep_lines` keeping lines
    /// recorded before it, or failing to keep the ones after.
    #[test]
    fn lines_are_kept_only_once_asked_for() {
        let mut t = Trace::new("t");
        t.record("before");
        assert_eq!(t.lines(), None);
        t.keep_lines();
        t.record("after");
        assert_eq!(
            t.lines(),
            Some(&["kbf-trace-format 1 t".to_owned(), "after".to_owned()][..])
        );
        t.keep_lines();
        assert_eq!(
            t.lines().map(<[String]>::len),
            Some(2),
            "asking twice keeps what was kept"
        );
    }
}
