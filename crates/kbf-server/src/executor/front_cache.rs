//! A small map bounded by its entries, for what `Front` remembers by digest: at the bound, the
//! entry remembered first goes first (as the memo of `kbf_front::action_tree` drops its entries).

use std::collections::BTreeMap;

/// A map of at most `bound` entries. Not shared: the owner holds it under a lock.
#[derive(Debug)]
pub(super) struct Bounded<K, V> {
    entries: BTreeMap<K, (V, u64)>,
    /// The keys by the order they were remembered in.
    order: BTreeMap<u64, K>,
    next: u64,
    bound: usize,
}

impl<K: Ord + Clone, V: Clone> Bounded<K, V> {
    /// A map of at most `bound` entries.
    pub(super) fn new(bound: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            order: BTreeMap::new(),
            next: 0,
            bound,
        }
    }

    /// The value remembered for `key`.
    pub(super) fn get(&self, key: &K) -> Option<V> {
        self.entries.get(key).map(|(value, _)| value.clone())
    }

    /// Remembers `value` for `key`, as the newest entry, and drops the oldest entries past the
    /// bound.
    pub(super) fn put(&mut self, key: K, value: V) {
        if let Some((_, old)) = self.entries.remove(&key) {
            self.order.remove(&old);
        }
        self.order.insert(self.next, key.clone());
        self.entries.insert(key, (value, self.next));
        self.next += 1;
        let excess = self.entries.len().saturating_sub(self.bound);
        let oldest: Vec<K> = (0..excess)
            .filter_map(|_| self.order.pop_first())
            .map(|(_, key)| key)
            .collect();
        for key in oldest {
            self.entries.remove(&key);
        }
    }
}
