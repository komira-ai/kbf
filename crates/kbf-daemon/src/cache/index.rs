//! The index (`docs/design/worker-cache.md#69-the-index`): every entry's key, size and
//! last use, in memory, and the one snapshot file it is saved to and loaded from.
//!
//! The snapshot is our own format, written whole each time: a 16-byte header (magic
//! `KBFCIDX\0`, the format version as a little-endian u32, 4 reserved bytes), the entry
//! count (u64), one 56-byte entry per key in key order (flags u8, bit 0 executable; 7
//! reserved; the SHA-256; size u64; last use in Unix seconds u64), then a CRC-32 of
//! every byte before it. Everything little-endian. A file of any other length, with
//! another magic, version or CRC, or that lists one key twice, is discarded whole.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::ops::Bound;
use std::path::Path;

use super::layout::Key;
use super::{Hooks, Point};

/// The snapshot's name in `v1/`, and its temporary name while it is written.
pub(crate) const SNAPSHOT: &str = "index.snap";
pub(crate) const SNAPSHOT_TMP: &str = "index.snap.tmp";

const MAGIC: &[u8; 8] = b"KBFCIDX\0";
const FORMAT: u32 = 1;
const HEADER: usize = 16;
const COUNT: usize = 8;
const ENTRY: usize = 56;
const CRC: usize = 4;

/// One key's line in the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub key: Key,
    pub last_use: u64,
}

/// Why a snapshot was not loaded (`cache_index_discarded`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discarded {
    /// There was none.
    Missing,
    /// Its length, CRC or entries are wrong.
    Corrupt,
    /// Its magic or format version is not this daemon's.
    Version,
}

impl Discarded {
    /// The word the daemon logs and reports.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::Version => "version",
        }
    }
}

/// The snapshot of `records`, which are in key order.
pub(crate) fn encode(records: &[Record]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + COUNT + records.len() * ENTRY + CRC);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&(records.len() as u64).to_le_bytes());
    for record in records {
        bytes.push(u8::from(record.key.executable));
        bytes.extend_from_slice(&[0; 7]);
        bytes.extend_from_slice(&record.key.hash);
        bytes.extend_from_slice(&record.key.size.to_le_bytes());
        bytes.extend_from_slice(&record.last_use.to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// The records of the snapshot `bytes`, or why it is discarded.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Record>, Discarded> {
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default());
    if bytes.len() < HEADER + COUNT + CRC {
        return Err(Discarded::Corrupt);
    }
    let (body, crc) = bytes.split_at(bytes.len() - CRC);
    if crc32fast::hash(body).to_le_bytes() != crc {
        return Err(Discarded::Corrupt);
    }
    if &bytes[..8] != MAGIC || bytes[8..12] != FORMAT.to_le_bytes() {
        return Err(Discarded::Version);
    }
    let count = u64_at(HEADER);
    let entries = (body.len() - HEADER - COUNT) as u64;
    if count.checked_mul(ENTRY as u64) != Some(entries) {
        return Err(Discarded::Corrupt);
    }
    let mut records = Vec::new();
    for entry in body[HEADER + COUNT..].chunks_exact(ENTRY) {
        let at = |i: usize| u64::from_le_bytes(entry[i..i + 8].try_into().unwrap_or_default());
        let mut hash = [0; 32];
        hash.copy_from_slice(&entry[8..40]);
        let key = Key {
            hash,
            size: at(40),
            executable: entry[0] & 1 == 1,
        };
        // Strictly ascending: a key listed twice, or out of order, is not ours.
        if records.last().is_some_and(|last: &Record| last.key >= key) {
            return Err(Discarded::Corrupt);
        }
        records.push(Record {
            key,
            last_use: at(48),
        });
    }
    Ok(records)
}

/// Writes `records` to `v1/index.snap` through `index.snap.tmp`: written, synced,
/// renamed over the old one, and the directory synced, so a crash leaves the old
/// snapshot or the new one, never a mix. On an error the temporary file is removed and
/// the old snapshot kept.
pub(crate) fn write(v1: &Path, records: &[Record], hooks: &dyn Hooks) -> io::Result<()> {
    let tmp = v1.join(SNAPSHOT_TMP);
    let written = write_tmp(&tmp, &encode(records), hooks)
        .and_then(|()| std::fs::rename(&tmp, v1.join(SNAPSHOT)))
        .and_then(|()| std::fs::File::open(v1)?.sync_all());
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn write_tmp(tmp: &Path, bytes: &[u8], hooks: &dyn Hooks) -> io::Result<()> {
    let mut file = std::fs::File::create(tmp)?;
    let (first, rest) = bytes.split_at(bytes.len() / 2);
    file.write_all(first)?;
    hooks.at(Point::SnapshotHalf);
    file.write_all(rest)?;
    file.sync_all()
}

/// One indexed entry's state in memory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    /// When a lease last used it, in Unix seconds (the snapshot keeps this).
    pub last_use: u64,
    /// Breaks ties between uses in one second, in the order they happened.
    pub seq: u64,
    /// Whether its bytes were hashed in this run.
    pub verified: bool,
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
    /// The index of a loaded snapshot: every entry unverified.
    pub fn loaded(records: &[Record]) -> Self {
        let mut index = Self::default();
        for record in records {
            index.insert(record.key, record.last_use, false);
        }
        index
    }

    pub fn get(&self, key: &Key) -> Option<Entry> {
        self.entries.get(key).copied()
    }

    /// Adds `key` used at `last_use`, counting its size; a key already there is left as
    /// it is. Whether it was added.
    pub fn insert(&mut self, key: Key, last_use: u64, verified: bool) -> bool {
        if self.entries.contains_key(&key) {
            return false;
        }
        self.seq += 1;
        let entry = Entry {
            last_use,
            seq: self.seq,
            verified,
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

    /// Records a use of `key` at `now`, and that its bytes were hashed if `verified`.
    pub fn touch(&mut self, key: &Key, now: u64, verified: bool) {
        if let Some(entry) = self.entries.get_mut(key) {
            self.lru.remove(&(entry.last_use, entry.seq));
            self.seq += 1;
            entry.last_use = now;
            entry.seq = self.seq;
            entry.verified |= verified;
            self.lru.insert((now, self.seq), *key);
        }
    }

    /// The keys, least recently used first.
    pub fn oldest(&self) -> impl Iterator<Item = &Key> {
        self.lru.values()
    }

    /// The indexed keys under the blobs directory `prefix`, in key order.
    pub fn under(&self, prefix: u8) -> Vec<Key> {
        let at = |byte: u8| {
            let mut hash = [0; 32];
            hash[0] = byte;
            Key {
                hash,
                size: 0,
                executable: false,
            }
        };
        let end = prefix
            .checked_add(1)
            .map_or(Bound::Unbounded, |next| Bound::Excluded(at(next)));
        let range = self.entries.range((Bound::Included(at(prefix)), end));
        range.map(|(key, _)| *key).collect()
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every entry as the snapshot records it, in key order.
    pub fn records(&self) -> Vec<Record> {
        (self.entries.iter())
            .map(|(key, entry)| Record {
                key: *key,
                last_use: entry.last_use,
            })
            .collect()
    }
}
