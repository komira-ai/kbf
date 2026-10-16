//! Index writes not yet applied, and the objects whose rows must be put back.
//!
//! [`Pending`] holds `ui/` row writes in memory, coalesced per key (a second write to a
//! row replaces the first), within a key and a byte bound. Past either, a new row is
//! refused and the caller counts it under `stage="index"`; nothing waits. No record is
//! lost by that: every pointer row names an object already written, and the writer
//! keeps that object's key in [`Kept`] and restores its rows from the object's footer
//! once the backlog drains, with no LIST.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kbf_objstore::ObjectKey;
use kbf_state::Cond;

/// An object whose index rows were dropped: its key and footer range (about 100 bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptObject {
    /// The object.
    pub key: ObjectKey,
    /// Where its footer starts.
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    pub footer_len: u32,
}

/// A row write waiting to be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The value.
    pub value: Vec<u8>,
    /// Its precondition: [`Cond::Absent`] for a pointer row (a row that exists already
    /// is the same row), [`Cond::Any`] for a header.
    pub cond: Cond,
    /// The object the row was made from, to keep if the row is dropped.
    pub origin: Option<KeptObject>,
}

impl Row {
    fn bytes(key: &str, row: &Row) -> usize {
        key.len() + row.value.len()
    }
}

/// The bounds of [`Pending`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingLimits {
    /// Distinct keys held at most.
    pub keys: usize,
    /// Key and value bytes held at most.
    pub bytes: usize,
}

impl Default for PendingLimits {
    /// 131,072 keys or 32 MiB.
    fn default() -> PendingLimits {
        PendingLimits {
            keys: 131_072,
            bytes: 32 * 1024 * 1024,
        }
    }
}

/// Row writes not yet applied, coalesced per key, in key order.
#[derive(Debug)]
pub struct Pending {
    rows: BTreeMap<String, Row>,
    bytes: usize,
    limits: PendingLimits,
}

impl Pending {
    /// Nothing pending.
    #[must_use]
    pub fn new(limits: PendingLimits) -> Pending {
        Pending {
            rows: BTreeMap::new(),
            bytes: 0,
            limits,
        }
    }

    /// Adds a write of `key`, replacing a pending write of the same key. A new key past
    /// either bound is refused and the row given back.
    ///
    /// # Errors
    /// The row, when it was not taken.
    pub fn insert(&mut self, key: String, row: Row) -> Result<(), Row> {
        let size = Row::bytes(&key, &row);
        if let Some(old) = self.rows.get(&key) {
            self.bytes = self.bytes - Row::bytes(&key, old) + size;
            self.rows.insert(key, row);
            return Ok(());
        }
        if self.rows.len() >= self.limits.keys || self.bytes + size > self.limits.bytes {
            return Err(row);
        }
        self.bytes += size;
        self.rows.insert(key, row);
        Ok(())
    }

    /// Takes up to `max_rows` writes whose bytes stay within `max_bytes` (always at least
    /// one, so one oversized row cannot wedge the queue), in key order.
    pub fn take(&mut self, max_rows: usize, max_bytes: usize) -> Vec<(String, Row)> {
        let mut out = Vec::new();
        let mut size = 0;
        while out.len() < max_rows {
            let Some(entry) = self.rows.first_entry() else {
                break;
            };
            let n = Row::bytes(entry.key(), entry.get());
            if !out.is_empty() && size + n > max_bytes {
                break;
            }
            size += n;
            self.bytes -= n;
            out.push(entry.remove_entry());
        }
        out
    }

    /// Writes pending.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether nothing is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Objects whose rows must be restored, oldest first, each once. Bounded: past the
/// bound the oldest falls off, to the weekly sweep that adopts orphans by LIST.
#[derive(Debug)]
pub struct Kept {
    order: VecDeque<KeptObject>,
    keys: BTreeSet<ObjectKey>,
    max: usize,
}

impl Kept {
    /// An empty list holding at most `max` objects.
    #[must_use]
    pub fn new(max: usize) -> Kept {
        Kept {
            order: VecDeque::new(),
            keys: BTreeSet::new(),
            max,
        }
    }

    /// Keeps `object` unless it is kept already. Returns the object that fell off the
    /// front to make room, if one did.
    pub fn push(&mut self, object: KeptObject) -> Option<KeptObject> {
        if self.max == 0 || !self.keys.insert(object.key.clone()) {
            return None;
        }
        let fell = if self.order.len() >= self.max {
            self.order.pop_front()
        } else {
            None
        };
        if let Some(f) = &fell {
            self.keys.remove(&f.key);
        }
        self.order.push_back(object);
        fell
    }

    /// Takes the oldest kept object.
    pub fn pop(&mut self) -> Option<KeptObject> {
        let object = self.order.pop_front()?;
        self.keys.remove(&object.key);
        Some(object)
    }

    /// Objects kept.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: &str) -> Row {
        Row {
            value: value.as_bytes().to_vec(),
            cond: Cond::Absent,
            origin: None,
        }
    }

    fn kept(name: &str) -> KeptObject {
        KeptObject {
            key: ObjectKey::new(format!("history/{name}")).unwrap(),
            footer_offset: 1,
            footer_len: 2,
        }
    }

    #[test]
    fn writes_coalesce_per_key_and_new_keys_stop_at_the_bounds() {
        // Catches: a second write of a row taking a key of its own (the bound would
        // fill with stale versions), or a bound that refuses a replacement.
        let mut p = Pending::new(PendingLimits {
            keys: 2,
            bytes: 100,
        });
        assert!(p.is_empty());
        p.insert("a".into(), row("1")).unwrap();
        p.insert("a".into(), row("22")).unwrap();
        assert_eq!(p.len(), 1);
        p.insert("b".into(), row("3")).unwrap();
        assert_eq!(p.insert("c".into(), row("4")), Err(row("4")));
        p.insert("b".into(), row("5")).unwrap();
        let taken = p.take(10, 1000);
        assert_eq!(taken, [("a".into(), row("22")), ("b".into(), row("5"))]);
        assert!(p.is_empty());
        let mut small = Pending::new(PendingLimits { keys: 10, bytes: 5 });
        small.insert("k".into(), row("1234")).unwrap();
        assert!(
            small.insert("j".into(), row("1")).is_err(),
            "the byte bound holds"
        );
        assert_eq!(small.take(1, 0).len(), 1);
        small.insert("j".into(), row("1")).unwrap();
    }

    #[test]
    fn take_bounds_rows_and_bytes_but_always_takes_one() {
        // Catches: a batch over the background transaction's row or byte cap, or an
        // oversized row that is never taken (the queue would wedge behind it).
        let mut p = Pending::new(PendingLimits::default());
        for k in ["a", "b", "c"] {
            p.insert(k.into(), row("xxxx")).unwrap();
        }
        assert_eq!(p.take(2, 1000).len(), 2);
        p.insert("a".into(), row("xxxx")).unwrap();
        let big: Vec<_> = p.take(10, 1).into_iter().map(|(k, _)| k).collect();
        assert_eq!(big, ["a"]);
        assert_eq!(p.take(10, 1000).len(), 1);
        assert!(p.take(10, 1000).is_empty());
    }

    #[test]
    fn kept_objects_are_unique_bounded_and_oldest_first() {
        // Catches: one object kept once per dropped row (re-read three times), an
        // unbounded list, or the newest falling off instead of the oldest.
        let mut k = Kept::new(2);
        assert_eq!(k.len(), 0);
        assert_eq!(k.push(kept("a")), None);
        assert_eq!(k.push(kept("a")), None);
        assert_eq!(k.len(), 1);
        assert_eq!(k.push(kept("b")), None);
        assert_eq!(k.push(kept("c")), Some(kept("a")));
        assert_eq!(
            k.push(kept("a")),
            Some(kept("b")),
            "a fell off, so it is new again"
        );
        assert_eq!(k.pop(), Some(kept("c")));
        assert_eq!(k.pop(), Some(kept("a")));
        assert_eq!(k.pop(), None);
        let mut none = Kept::new(0);
        assert_eq!(none.push(kept("a")), None);
        assert_eq!(none.len(), 0);
    }
}
