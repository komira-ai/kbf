//! The place memo: where each blob a tree named was found, so that the next tree naming it asks
//! neither the pack index nor `cas/l/`.
//!
//! A blob is named by its digest, so what is remembered of it (the pack span its bytes are, or
//! that `cas/l/<sha256>` holds them whole) is never wrong about the bytes, only possibly out of
//! date about where they are kept. Each entry is therefore good for [`MEMO_TTL`] from the moment
//! it was made, on the writer's clock, like the manifest memo's. The memo holds at most a bound
//! of entries and drops the oldest first.

use std::collections::BTreeMap;
use std::time::SystemTime;

use kbf_types::Digest;

use super::MEMO_TTL;
use super::place::Place;

/// The most blob places the memo remembers.
pub const PLACE_ENTRIES: usize = 1_000_000;

/// The memo. Not shared: the writer holds it under a lock.
#[derive(Debug)]
pub(super) struct Places {
    entries: BTreeMap<Digest, (Place, SystemTime, u64)>,
    /// The digests by the order they were remembered in.
    order: BTreeMap<u64, Digest>,
    next: u64,
    bound: usize,
}

impl Places {
    /// A memo of at most `bound` entries.
    pub(super) const fn new(bound: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            next: 0,
            bound,
        }
    }

    /// The places of `blobs` that are remembered and good at `now`, and the rest.
    pub(super) fn split(
        &self,
        blobs: &[Digest],
        now: SystemTime,
    ) -> (BTreeMap<Digest, Place>, Vec<Digest>) {
        let mut known = BTreeMap::new();
        let mut unknown = Vec::new();
        for digest in blobs {
            match self.entries.get(digest) {
                Some((place, expires, _)) if now < *expires => {
                    known.insert(*digest, place.clone());
                }
                _ => unknown.push(*digest),
            }
        }
        (known, unknown)
    }

    /// Remembers `found`, each good until [`MEMO_TTL`] after `now`, dropping the oldest entries
    /// past the bound.
    pub(super) fn remember(
        &mut self,
        found: impl Iterator<Item = (Digest, Place)>,
        now: SystemTime,
    ) {
        let expires = now.checked_add(MEMO_TTL).unwrap_or(now);
        for (digest, place) in found {
            if let Some((_, _, old)) = self.entries.insert(digest, (place, expires, self.next)) {
                self.order.remove(&old);
            }
            self.order.insert(self.next, digest);
            self.next += 1;
        }
        let excess = self.entries.len().saturating_sub(self.bound);
        let oldest: Vec<Digest> = (0..excess)
            .filter_map(|_| self.order.pop_first())
            .map(|(_, digest)| digest)
            .collect();
        for digest in oldest {
            self.entries.remove(&digest);
        }
    }

    /// Drops every entry that places a blob in the pack `key`. Returns how many it dropped.
    pub(super) fn forget_pack(&mut self, key: &str) -> usize {
        let before = self.entries.len();
        let order = &mut self.order;
        self.entries.retain(|_, (place, _, at)| {
            let named = matches!(place, Place::Packed { key: pack, .. } if pack == key);
            if named {
                order.remove(at);
            }
            !named
        });
        before - self.entries.len()
    }

    /// Drops every entry.
    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}
