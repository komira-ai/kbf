//! The index (`docs/design/worker-cache.md#69-the-index`): every entry's key, size and
//! last use, in memory, in least-recently-used order, with the byte total the budget
//! is enforced against.

use std::collections::BTreeMap;

use super::layout::Key;

/// One indexed entry's state in memory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    /// When a lease last used it, in Unix seconds.
    pub last_use: u64,
    /// Breaks ties between uses in one second, in the order they happened.
    pub seq: u64,
}

/// The entries in memory: by key, in least-recently-used order, and their byte total.
#[derive(Debug, Default)]
pub(crate) struct Index {
    entries: BTreeMap<Key, Entry>,
    lru: BTreeMap<(u64, u64), Key>,
    total: u64,
    seq: u64,
}

impl Index {
    pub fn get(&self, key: &Key) -> Option<Entry> {
        self.entries.get(key).copied()
    }

    /// Adds `key` used at `last_use`, counting its size; a key already there is left as
    /// it is. Whether it was added.
    pub fn insert(&mut self, key: Key, last_use: u64) -> bool {
        if self.entries.contains_key(&key) {
            return false;
        }
        self.seq += 1;
        let entry = Entry {
            last_use,
            seq: self.seq,
        };
        self.entries.insert(key, entry);
        self.lru.insert((last_use, self.seq), key);
        self.total += key.size;
        true
    }

    /// Removes `key`, uncounting its size. Whether it was there.
    pub fn remove(&mut self, key: &Key) -> bool {
        let Some(entry) = self.entries.remove(key) else {
            return false;
        };
        self.lru.remove(&(entry.last_use, entry.seq));
        self.total -= key.size;
        true
    }

    /// Records a use of `key` at `now`.
    pub fn touch(&mut self, key: &Key, now: u64) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.lru.remove(&(entry.last_use, entry.seq));
            self.seq += 1;
            entry.last_use = now;
            entry.seq = self.seq;
            self.lru.insert((now, self.seq), *key);
        }
    }

    /// The keys, least recently used first.
    pub fn oldest(&self) -> impl Iterator<Item = &Key> {
        self.lru.values()
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}
