//! Where blobs lie, kept by the daemon so that a blob it has read through the tierstore once is
//! read again without asking the frontend.
//!
//! [`Placed`] is the frontend's pack index ([`PackIndex`], `kbf_daemon::RemoteIndex` in the
//! daemon) with the places it answered kept in memory: a `locate` of a blob whose place is kept
//! is answered from it, and the frontend is asked about the others only. A place is the key of a
//! pack and a range of it; a pack is written once under its key and never changed, so a kept
//! place stays true for as long as the pack exists. When the pack is gone (garbage collected) or
//! the bytes read from it are not the blob, [`TierReads`] forgets the place, and the next read of
//! the blob asks the frontend again. So a warm read through the tierstore touches neither the
//! frontend's CAS nor its index, and goes on while the frontend's index cannot be asked.
//!
//! At most [`MAX_PLACED`] places are kept; past that the oldest kept goes first. A place is about
//! a hundred bytes, so the most they take is a few MiB.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_daemon::PackReads;
use kbf_objstore::ObjectStore;
use kbf_pack::{Fetch, ReadError, Reader};
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackIndex, PackIndexError, PackInfo, Recorded,
};

/// The most places [`Placed`] keeps.
pub(crate) const MAX_PLACED: usize = 1 << 16;

/// The kept places, and the order they were kept in.
#[derive(Default)]
struct Kept {
    at: BTreeMap<BlobDigest, Location>,
    order: VecDeque<BlobDigest>,
}

impl Kept {
    /// Keeps `place` for `digest`, the oldest place going if `limit` are kept.
    fn keep(&mut self, digest: BlobDigest, place: Location, limit: usize) {
        if self.at.insert(digest, place).is_some() {
            return;
        }
        self.order.push_back(digest);
        let excess = self.order.len().saturating_sub(limit);
        for oldest in self.order.drain(..excess) {
            self.at.remove(&oldest);
        }
    }
}

/// The frontend's pack index, with the places it answered kept (the module docs).
pub(crate) struct Placed {
    inner: Arc<dyn PackIndex>,
    kept: Mutex<Kept>,
    limit: usize,
}

impl Placed {
    /// `inner`, keeping at most [`MAX_PLACED`] places.
    pub(crate) fn new(inner: Arc<dyn PackIndex>) -> Self {
        Self::keeping(inner, MAX_PLACED)
    }

    /// `inner`, keeping at most `limit` places.
    pub(crate) fn keeping(inner: Arc<dyn PackIndex>, limit: usize) -> Self {
        Self {
            inner,
            kept: Mutex::default(),
            limit,
        }
    }

    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets the place kept for `digest`, if one is.
    pub(crate) fn forget(&self, digest: &BlobDigest) {
        let mut kept = self.kept();
        if kept.at.remove(digest).is_some() {
            kept.order.retain(|held| held != digest);
        }
    }

    /// How many places are kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.kept().at.len()
    }
}

impl PackIndex for Placed {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.inner.record_pack(pack)
    }

    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }

    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        let mut found: Vec<Option<Location>> = {
            let kept = self.kept();
            digests.iter().map(|d| kept.at.get(d).cloned()).collect()
        };
        let unknown: Vec<BlobDigest> = digests
            .iter()
            .zip(&found)
            .filter(|(_, place)| place.is_none())
            .map(|(digest, _)| *digest)
            .collect();
        if unknown.is_empty() {
            return Ok(found);
        }
        let mut asked = self.inner.locate(&unknown)?.into_iter();
        let mut kept = self.kept();
        for (digest, place) in digests.iter().zip(found.iter_mut()) {
            if place.is_some() {
                continue;
            }
            *place = asked.next().flatten();
            if let Some(answered) = place {
                kept.keep(*digest, answered.clone(), self.limit);
            }
        }
        Ok(found)
    }

    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        self.inner.find_missing(digests)
    }

    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        self.inner.touch(digests)
    }

    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        self.inner.dead_packs(grace, limit)
    }

    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        self.inner.unreferenced_keys(keys)
    }
}

/// The reader of the packs through the tierstore, forgetting the kept place of each blob whose
/// pack is gone or whose bytes are not the blob, so that its next read asks the frontend again.
pub(crate) struct TierReads<S: ObjectStore> {
    reader: Reader<S>,
    placed: Arc<Placed>,
}

impl<S: ObjectStore> TierReads<S> {
    /// `reader`, which finds blobs through `placed`.
    pub(crate) fn new(reader: Reader<S>, placed: Arc<Placed>) -> Self {
        Self { reader, placed }
    }
}

impl<S: ObjectStore> PackReads for TierReads<S> {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        Box::pin(async move {
            let fetched = self.reader.read(digests).await?;
            for (digest, fetch) in digests.iter().zip(&fetched) {
                if matches!(fetch, Fetch::Lost | Fetch::Damaged) {
                    self.placed.forget(digest);
                }
            }
            Ok(fetched)
        })
    }
}

#[cfg(test)]
mod tests;
