//! The content cache of decoded Directories behind the input walk: [`DirCache`].
//!
//! A digest names its content, so what a Directory blob says (the digests of the files
//! and subdirectories it names) never goes stale and needs no epoch or time to live.
//! What does change is whether the blob is still *held*; the cache says nothing of that,
//! and the walk (`input_walk`) asks the store (FindMissing) about every cached Directory
//! it relies on, so an evicted or lost blob is still found.
//!
//! - **Bounded by bytes.** Each entry costs its digests plus [`ENTRY_OVERHEAD`]
//!   ([`Decoded::cost`]): names and modes are not kept. [`DIR_CACHE_BYTES`] sizes it.
//! - **Least recently used out first**, a hit counting as a use.
//! - **Shared by every Execute**, with a flight per digest: a Directory two Executes
//!   reach at once is read by the first, and the second waits for that read instead of
//!   starting its own ([`Lookup::Follow`]). A reader that is dropped, or fails, ends its
//!   flight and the waiters look again, so none waits on a read that is gone.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kbf_types::Digest;
use tokio::sync::Semaphore;

use crate::input_tree::Decoded;

/// The most bytes of decoded Directories the walk keeps. A komira-like tree is about
/// 1060 Directories and 15.5k files: a digest is 40 bytes, so its 15.5k file digests and
/// 1.1k subdirectory digests cost about 0.67 MB, and the 1060 entries' overhead
/// ([`ENTRY_OVERHEAD`], 160 bytes each) another 0.17 MB: about 0.84 MB a tree. 128 MiB
/// holds some 150 distinct trees of that size, where a farm's trees share most of their
/// Directories (a few toolchains, and a source tree per project).
///
/// [`ENTRY_OVERHEAD`]: crate::input_tree::ENTRY_OVERHEAD
pub(crate) const DIR_CACHE_BYTES: u64 = 128 << 20;

/// The decoded Directories, least recently used first, and the reads in progress.
pub(crate) struct DirCache {
    inner: Mutex<State>,
    capacity: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<Digest, Entry>,
    /// Recency tick to digest; the first key is the least recently used.
    order: BTreeMap<u64, Digest>,
    /// A read in progress: closed when it ends, however it ends.
    flights: HashMap<Digest, Arc<Semaphore>>,
    tick: u64,
    bytes: u64,
}

struct Entry {
    decoded: Arc<Decoded>,
    cost: u64,
    used: u64,
}

/// What [`DirCache::lookup`] found.
pub(crate) enum Lookup<'a> {
    /// The Directory is cached.
    Cached(Arc<Decoded>),
    /// Nobody is reading it: the caller reads it and calls [`Lead::finish`].
    Lead(Lead<'a>),
    /// Another walk is reading it: wait for the gate to close, then look again.
    Follow(Arc<Semaphore>),
}

/// The right to read one Directory for everyone who wants it. Dropped without
/// [`Lead::finish`] (a failed or cancelled read), it ends the flight and caches nothing.
pub(crate) struct Lead<'a> {
    cache: &'a DirCache,
    digest: Digest,
    /// `None` when the cache is off: nothing is shared.
    gate: Option<Arc<Semaphore>>,
}

impl Lead<'_> {
    /// Caches `decoded` as the content of the digest and ends the flight. The number of
    /// entries this pushed out.
    pub(crate) fn finish(self, decoded: Arc<Decoded>) -> u64 {
        self.cache.insert(self.digest, decoded)
    }
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            self.cache.lock().flights.remove(&self.digest);
            gate.close();
        }
    }
}

impl Default for DirCache {
    fn default() -> Self {
        Self::new(DIR_CACHE_BYTES)
    }
}

impl DirCache {
    /// A cache of at most `capacity` bytes. Zero keeps nothing and shares no reads.
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            inner: Mutex::new(State::default()),
            capacity,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every change leaves the maps agreeing before it can panic.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached Directory, a read in progress to wait for, or the lead of a new read.
    /// A hit counts as a use.
    pub(crate) fn lookup(&self, digest: &Digest) -> Lookup<'_> {
        if self.capacity == 0 {
            return Lookup::Lead(Lead {
                cache: self,
                digest: *digest,
                gate: None,
            });
        }
        let mut state = self.lock();
        if let Some(gate) = state.flights.get(digest) {
            return Lookup::Follow(Arc::clone(gate));
        }
        let tick = state.tick;
        if let Some(entry) = state.entries.get_mut(digest) {
            let old = std::mem::replace(&mut entry.used, tick);
            let decoded = Arc::clone(&entry.decoded);
            state.order.remove(&old);
            state.order.insert(tick, *digest);
            state.tick += 1;
            return Lookup::Cached(decoded);
        }
        let gate = Arc::new(Semaphore::new(0));
        state.flights.insert(*digest, Arc::clone(&gate));
        Lookup::Lead(Lead {
            cache: self,
            digest: *digest,
            gate: Some(gate),
        })
    }

    /// Keeps `decoded` as the content of `digest`, pushing out the least recently used
    /// beyond the bound; one costing more than the whole bound is not kept. The number
    /// of entries pushed out.
    fn insert(&self, digest: Digest, decoded: Arc<Decoded>) -> u64 {
        let cost = decoded.cost();
        if cost > self.capacity {
            return 0;
        }
        let mut state = self.lock();
        let tick = state.tick;
        state.tick += 1;
        let entry = Entry {
            decoded,
            cost,
            used: tick,
        };
        if let Some(old) = state.entries.insert(digest, entry) {
            state.order.remove(&old.used);
            state.bytes -= old.cost;
        }
        state.order.insert(tick, digest);
        state.bytes += cost;
        let mut evicted = 0;
        while state.bytes > self.capacity {
            let oldest = state.order.pop_first().map(|(_, digest)| digest);
            // Nothing left to push out (the maps disagreeing) ends the loop with the
            // bytes at zero rather than spinning.
            let freed = oldest
                .and_then(|digest| state.entries.remove(&digest))
                .map_or(state.bytes, |gone| gone.cost);
            state.bytes -= freed;
            evicted += u64::from(oldest.is_some());
        }
        evicted
    }

    /// Bytes held now.
    pub(crate) fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    /// Entries held now: for tests.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().entries.len()
    }
}

#[cfg(test)]
#[path = "dir_cache_tests.rs"]
mod tests;
