//! A cache of the decoded `Directory` messages of input trees, for the put-directory closure
//! walk ([`crate::prepare_run::collect_closure_keeping`]'s cached form).
//!
//! A `Directory` is named by the digest of its bytes, so what is kept under a digest is never
//! stale: there is nothing to expire and nothing to invalidate. The cache says nothing about
//! whether a blob is still held. The walk still asks the store (`FindMissing`) about the
//! Command and every file of the tree, as it does without a cache; what the cache saves is the
//! read and the decode of each `Directory`.
//!
//! - **Memory.** The decoded [`KeptDirectory`] of each digest, bounded by bytes (a cost worked
//!   out from the names, the digests and a fixed overhead per `Directory` and per node), the
//!   least recently used out first. A `Directory` costing more than the bound is not kept.
//! - **Disk (optional).** The `Directory` blob as the store gave it, in a
//!   [`kbf_pack::ReadCache`] directory bounded by bytes, so a restarted frontend finds the
//!   trees it had. A blob is written only after it decoded, and the cache checks its SHA-256 on
//!   every read; a file that is not the blob is dropped and the `Directory` is read from the
//!   store again. A failure to write a blob is not an error of the walk: the `Directory` stays
//!   in memory and the walk goes on. The cache does blocking file I/O, so each call runs on the
//!   blocking pool.
//! - **Never the empty `Directory`.** The empty blob is the empty `Directory` and costs no
//!   read; it is not stored.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use kbf_pack::{CacheError, ReadCache};
use kbf_pack_index::BlobDigest;
use kbf_types::Digest;

use super::closure::KeptDirectory;

/// The memory bound the frontend uses: 256 MiB.
pub const DIRECTORY_MEMORY_BYTES: u64 = 256 << 20;

/// The cost of one `Directory` apart from its nodes.
const DIRECTORY_OVERHEAD: u64 = 256;

/// The cost of one node apart from its name: its digest and its place in a list.
const NODE_OVERHEAD: u64 = 64;

/// A bounded cache of decoded `Directory` messages by digest (the module docs).
pub struct DirectoryCache {
    bound: u64,
    state: Mutex<State>,
    disk: Option<Arc<ReadCache>>,
}

#[derive(Debug, Default)]
struct State {
    /// Each kept `Directory` with the tick it was last used at and its cost.
    entries: BTreeMap<Digest, (KeptDirectory, u64, u64)>,
    /// The digests by the tick they were last used at: the first is the least recently used.
    order: BTreeMap<u64, Digest>,
    next: u64,
    used: u64,
}

impl fmt::Debug for DirectoryCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectoryCache")
            .field("memory_bound", &self.bound)
            .field("disk", &self.disk.is_some())
            .finish_non_exhaustive()
    }
}

impl DirectoryCache {
    /// A cache in memory alone, holding at most `memory_bytes` by the cost of its entries.
    #[must_use]
    pub fn in_memory(memory_bytes: u64) -> Self {
        Self {
            bound: memory_bytes,
            state: Mutex::new(State::default()),
            disk: None,
        }
    }

    /// A cache in memory (at most `memory_bytes`) and in the local directory `dir`, which holds
    /// at most `disk_bytes` of `Directory` blobs. The blobs already there are kept.
    ///
    /// # Errors
    /// [`CacheError::Io`] when `dir` cannot be made or listed.
    pub fn open(
        dir: impl Into<PathBuf>,
        memory_bytes: u64,
        disk_bytes: u64,
    ) -> Result<Self, CacheError> {
        let disk = ReadCache::open(dir, disk_bytes)?;
        Ok(Self {
            disk: Some(Arc::new(disk)),
            ..Self::in_memory(memory_bytes)
        })
    }

    /// The kept `Directory` `digest`, which becomes the most recently used.
    pub(crate) fn recent(&self, digest: &Digest) -> Option<KeptDirectory> {
        let mut guard = self.lock();
        let State {
            entries,
            order,
            next,
            ..
        } = &mut *guard;
        let (kept, tick, _) = entries.get_mut(digest)?;
        order.remove(tick);
        *tick = *next;
        order.insert(*next, *digest);
        *next += 1;
        Some(kept.clone())
    }

    /// Keeps `kept` as the `Directory` `digest` in memory, dropping the least recently used
    /// until the cache is within its bound. A `Directory` over the bound is not kept, and one
    /// held already stays as it is.
    pub(crate) fn remember(&self, digest: Digest, kept: &KeptDirectory) {
        let cost = cost(kept);
        if cost > self.bound {
            return;
        }
        let mut guard = self.lock();
        let State {
            entries,
            order,
            next,
            used,
        } = &mut *guard;
        if entries.contains_key(&digest) {
            return;
        }
        entries.insert(digest, (kept.clone(), *next, cost));
        order.insert(*next, digest);
        *next += 1;
        *used += cost;
        while let Some((_, oldest)) = (*used > self.bound).then(|| order.pop_first()).flatten() {
            let freed = entries.remove(&oldest).map_or(0, |(_, _, freed)| freed);
            *used = used.saturating_sub(freed);
        }
    }

    /// The `Directory` blob `digest` from the disk half, checked against its digest: `None`
    /// when there is no disk half or it does not hold the blob.
    pub(crate) async fn stored(&self, digest: &Digest) -> Option<Bytes> {
        let disk = Arc::clone(self.disk.as_ref()?);
        let key = to_index(digest);
        tokio::task::spawn_blocking(move || disk.get(&key))
            .await
            .ok()
            .flatten()
    }

    /// Keeps the `Directory` blob `blob` under `digest` on the disk half, if there is one. A
    /// blob it cannot write is left out; the walk does not depend on it.
    pub(crate) async fn store(&self, digest: &Digest, blob: &Bytes) {
        let Some(disk) = self.disk.as_ref().map(Arc::clone) else {
            return;
        };
        let key = to_index(digest);
        let blob = blob.clone();
        let _ = tokio::task::spawn_blocking(move || disk.put(&key, &blob)).await;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The index's form of a digest.
const fn to_index(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// What `kept` costs in memory: an overhead for the `Directory`, and for each node an overhead
/// (its digest and its slot) and the bytes of its name (and of a symlink's target).
fn cost(kept: &KeptDirectory) -> u64 {
    let len = |text: &str| u64::try_from(text.len()).unwrap_or(u64::MAX);
    let files = kept.files.iter().map(|(name, _, _)| len(name));
    let directories = kept.directories.iter().map(|(name, _)| len(name));
    let symlinks = kept
        .symlinks
        .iter()
        .map(|(name, target)| len(name).saturating_add(len(target)));
    files
        .chain(directories)
        .chain(symlinks)
        .fold(DIRECTORY_OVERHEAD, |total, bytes| {
            total.saturating_add(NODE_OVERHEAD).saturating_add(bytes)
        })
}

#[cfg(test)]
#[path = "directories_tests.rs"]
mod tests;
