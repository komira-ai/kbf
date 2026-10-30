//! What the cache holds, in memory: each object's size and tag, and the order of last use.
//!
//! The order is a counter, not a time: an object that is used takes the next number, and the
//! oldest use is the lowest. The files' times are only the seed of the order at start and a
//! record kept for the next one (see [`Index::take_touched`]).

use std::collections::{BTreeMap, HashMap};

use crate::Meta;

/// One object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub size: u64,
    pub tag: String,
    pub modified: u64,
    /// The place in the order of use.
    used: u64,
}

impl Entry {
    /// What a HEAD says of the object.
    pub(super) fn meta(&self) -> Meta {
        Meta {
            size: self.size,
            tag: self.tag.clone(),
            modified: self.modified,
        }
    }
}

/// The objects of the cache, by name, and in the order of their last use.
#[derive(Debug, Default)]
pub(super) struct Index {
    entries: HashMap<String, Entry>,
    order: BTreeMap<u64, String>,
    next: u64,
    bytes: u64,
    /// The objects used since the last [`Index::take_touched`], with the time of the last use.
    touched: HashMap<String, u64>,
}

impl Index {
    /// The bytes of all the objects.
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The number of objects.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The object `hex`, without counting it as used.
    pub(super) fn get(&self, hex: &str) -> Option<&Entry> {
        self.entries.get(hex)
    }

    /// Adds the object `hex`, as the most recently used; replaces one of that name.
    pub(super) fn insert(&mut self, hex: &str, size: u64, tag: String, modified: u64) {
        self.remove(hex);
        let used = self.next;
        self.next += 1;
        let entry = Entry {
            size,
            tag,
            modified,
            used,
        };
        self.order.insert(used, hex.to_owned());
        self.entries.insert(hex.to_owned(), entry);
        self.bytes += size;
    }

    /// Takes the object out; gives it if it was there.
    pub(super) fn remove(&mut self, hex: &str) -> Option<Entry> {
        let entry = self.entries.remove(hex)?;
        self.order.remove(&entry.used);
        self.touched.remove(hex);
        self.bytes -= entry.size;
        Some(entry)
    }

    /// Counts the object as used at `now`, and gives its size, if it is there.
    pub(super) fn used(&mut self, hex: &str, now: u64) -> Option<u64> {
        let entry = self.entries.get_mut(hex)?;
        self.order.remove(&entry.used);
        entry.used = self.next;
        self.next += 1;
        self.order.insert(entry.used, hex.to_owned());
        self.touched.insert(hex.to_owned(), now);
        Some(entry.size)
    }

    /// The names of the objects, in order.
    pub(super) fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.keys().cloned().collect();
        names.sort();
        names
    }

    /// The names and sizes of the objects, the least recently used first.
    pub(super) fn oldest_first(&self) -> Vec<(String, u64)> {
        let size = |hex: &String| self.entries.get(hex).map_or(0, |entry| entry.size);
        self.order
            .values()
            .map(|hex| (hex.clone(), size(hex)))
            .collect()
    }

    /// The objects used since the last call, with the time of the last use of each.
    pub(super) fn take_touched(&mut self) -> Vec<(String, u64)> {
        let mut touched: Vec<(String, u64)> = self.touched.drain().collect();
        touched.sort();
        touched
    }
}
