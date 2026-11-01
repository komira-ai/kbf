//! The in-memory pack index: the rules of the Postgres one over maps, for tests.
//!
//! Test support (feature `test-support`). It keeps the first location of a blob, answers a
//! pack recorded twice with the same contents as `created: false`, and refuses a record of
//! a key with other contents, exactly as the table-backed index does; the conformance suite
//! runs against both.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::conformance::PackBackend;
use crate::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded, check_batch,
    count, micros, micros_of,
};
use kbf_state::{Clock, ManualClock};

/// A pack index in memory. A handle made by [`MemoryPackIndex::handle`] shares the index.
pub struct MemoryPackIndex {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Clock>,
}

#[derive(Default)]
struct State {
    /// The id of the last pack recorded: packs are listed in the order they were recorded.
    last_id: u64,
    packs: BTreeMap<String, PackRow>,
    blobs: BTreeMap<BlobDigest, BlobRow>,
}

struct PackRow {
    id: u64,
    info: PackInfo,
}

struct BlobRow {
    pack: String,
    offset: u64,
    last_touch_us: i64,
}

impl MemoryPackIndex {
    /// An empty index that reads the time from `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> MemoryPackIndex {
        MemoryPackIndex {
            state: Arc::default(),
            clock,
        }
    }

    /// Another handle on the same index, as a second connection to a database is.
    #[must_use]
    pub fn handle(&self) -> MemoryPackIndex {
        MemoryPackIndex {
            state: Arc::clone(&self.state),
            clock: Arc::clone(&self.clock),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PackIndex for MemoryPackIndex {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        let blobs = pack.checked()?;
        let now = micros_of(self.clock.now());
        let mut guard = self.lock();
        let state = &mut *guard;
        if let Some(row) = state.packs.get(&pack.key) {
            // The blobs this record names that the index holds in this pack, at the offset
            // named: for a pack recorded with these contents, all it pointed at. A blob the
            // index held in another pack when the pack was recorded is not compared.
            let mut matched = 0_u32;
            for blob in &blobs {
                let held = state.blobs.get(&blob.digest);
                let here = held.map(|b| (b.pack.as_str(), b.offset));
                if here == Some((pack.key.as_str(), blob.offset)) {
                    matched += 1;
                }
            }
            let info = &row.info;
            let blob_count = count(blobs.len());
            let declared = (
                info.writer.as_str(),
                info.size,
                info.blob_count,
                info.live_blobs,
            );
            let offered = (pack.writer.as_str(), pack.size, blob_count, matched);
            if declared != offered {
                return Err(PackIndexError::Conflict {
                    key: pack.key.clone(),
                });
            }
            return Ok(Recorded {
                created: false,
                indexed: info.live_blobs,
            });
        }
        state.last_id += 1;
        let mut indexed = 0_u32;
        for blob in &blobs {
            if let Entry::Vacant(slot) = state.blobs.entry(blob.digest) {
                slot.insert(BlobRow {
                    pack: pack.key.clone(),
                    offset: blob.offset,
                    last_touch_us: now,
                });
                indexed += 1;
            }
        }
        let info = PackInfo {
            key: pack.key.clone(),
            writer: pack.writer.clone(),
            size: pack.size,
            created_us: now,
            blob_count: count(blobs.len()),
            live_blobs: indexed,
        };
        let row = PackRow {
            id: state.last_id,
            info,
        };
        state.packs.insert(pack.key.clone(), row);
        Ok(Recorded {
            created: true,
            indexed,
        })
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        let state = self.lock();
        Ok(state.packs.get(key).map(|row| row.info.clone()))
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        check_batch(digests.len())?;
        let state = self.lock();
        let locate = |digest: &BlobDigest| {
            let row = state.blobs.get(digest)?;
            Some(Location {
                pack_key: row.pack.clone(),
                offset: row.offset,
                length: digest.size,
                last_touch_us: row.last_touch_us,
            })
        };
        Ok(digests.iter().map(locate).collect())
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        check_batch(digests.len())?;
        let state = self.lock();
        let missing = digests.iter().filter(|d| !state.blobs.contains_key(*d));
        Ok(missing.copied().collect())
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        check_batch(digests.len())?;
        let now = micros_of(self.clock.now());
        let distinct: BTreeSet<&BlobDigest> = digests.iter().collect();
        let mut state = self.lock();
        let mut touched = 0;
        for digest in distinct {
            if let Some(row) = state.blobs.get_mut(digest) {
                row.last_touch_us = now;
                touched += 1;
            }
        }
        Ok(touched)
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        let cutoff = micros_of(self.clock.now()).saturating_sub(micros(grace));
        let state = self.lock();
        let held: BTreeSet<&str> = state.blobs.values().map(|b| b.pack.as_str()).collect();
        let mut dead: Vec<&PackRow> = state.packs.values().collect();
        dead.retain(|p| p.info.created_us < cutoff);
        dead.retain(|p| !held.contains(p.info.key.as_str()));
        dead.sort_by_key(|p| p.id);
        let dead = dead.into_iter().take(limit);
        Ok(dead.map(|p| p.info.clone()).collect())
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        check_batch(keys.len())?;
        let state = self.lock();
        let unknown = keys.iter().filter(|k| !state.packs.contains_key(*k));
        Ok(unknown.cloned().collect())
    }
}

/// Opens memory indexes for the conformance suite.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryPackBackend;

impl PackBackend for MemoryPackBackend {
    type Index = MemoryPackIndex;

    fn open_index(&self, clock: Arc<ManualClock>) -> MemoryPackIndex {
        MemoryPackIndex::new(clock)
    }

    fn reconnect(&self, index: &MemoryPackIndex) -> MemoryPackIndex {
        index.handle()
    }
}
