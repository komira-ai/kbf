//! The memo: a `Directory` digest to its manifest, so that a subtree two input roots share (a
//! toolchain) is built and stored once.
//!
//! - **Bounded by entries**: at the bound, the entry remembered first goes first.
//! - **Expiring**: an entry is good until a fixed time after it was made ([`super::MEMO_TTL`])
//!   and no later than the earliest-expiring of its subdirectories, so that a hit never names a
//!   subtree whose places are older than that. An expired entry is a miss; it stays until it is
//!   remembered anew, forgotten or dropped at the bound, so that [`Memo::forget`] still walks
//!   through it.
//! - **Forgotten by root**: [`Memo::forget`] drops an input root's entry and every entry it
//!   reaches through its subdirectories, so that the next tree of that root is built from the
//!   pack index as it is then.

use std::collections::BTreeMap;
use std::time::SystemTime;

use kbf_types::Digest;

/// What a built `Directory`'s subtree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Measure {
    /// The entries below it, across all levels, a subtree named twice counted twice (as
    /// tierstore counts them).
    pub(super) entries: u64,
    /// The levels below it: the number of names on the path to its deepest entry; 0 for an
    /// empty directory.
    pub(super) height: usize,
    /// When it stops being good.
    pub(super) expires: SystemTime,
}

/// A remembered manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Remembered {
    /// The manifest's digest (its length is its size).
    pub(super) manifest: Digest,
    /// Its subtree.
    pub(super) measure: Measure,
    /// The `Directory` digests of its subdirectories.
    pub(super) children: Vec<Digest>,
}

/// The memo. Not shared: the writer holds it under a lock.
#[derive(Debug)]
pub(super) struct Memo {
    entries: BTreeMap<Digest, (Remembered, u64)>,
    /// The digests by the order they were remembered in.
    order: BTreeMap<u64, Digest>,
    next: u64,
    bound: usize,
}

impl Memo {
    /// A memo of at most `bound` entries.
    pub(super) const fn new(bound: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            next: 0,
            bound,
        }
    }

    /// The manifest of the `Directory` `digest`, if one is remembered and good at `now`.
    pub(super) fn get(&mut self, digest: &Digest, now: SystemTime) -> Option<Remembered> {
        let (remembered, _) = self.entries.get(digest)?;
        (now < remembered.measure.expires).then(|| remembered.clone())
    }

    /// Remembers `remembered` for the `Directory` `digest`, dropping the oldest entries past
    /// the bound.
    pub(super) fn put(&mut self, digest: Digest, remembered: Remembered) {
        self.remove(&digest);
        self.entries.insert(digest, (remembered, self.next));
        self.order.insert(self.next, digest);
        self.next += 1;
        let excess = self.entries.len().saturating_sub(self.bound);
        let oldest: Vec<Digest> = (0..excess)
            .filter_map(|_| self.order.pop_first())
            .map(|(_, digest)| digest)
            .collect();
        for digest in oldest {
            self.entries.remove(&digest);
        }
    }

    /// Drops the entry of `root` and every entry reachable from it, expired ones included.
    pub(super) fn forget(&mut self, root: &Digest) {
        let mut stack = vec![*root];
        while let Some(digest) = stack.pop() {
            if let Some((remembered, _)) = self.remove(&digest) {
                stack.extend(remembered.children);
            }
        }
    }

    fn remove(&mut self, digest: &Digest) -> Option<(Remembered, u64)> {
        let removed = self.entries.remove(digest)?;
        self.order.remove(&removed.1);
        Some(removed)
    }
}
