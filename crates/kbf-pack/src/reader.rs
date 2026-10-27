//! The reader: blobs by digest, from the read cache or from the packs the index names.
//!
//! A read asks the cache first. For what the cache lacks it asks the index where each blob
//! is, groups the blobs by pack, and reads each pack with as few ranged requests as it can:
//! blobs that lie close together in a pack (within [`ReaderConfig::gap`] bytes) are one
//! range. Every blob is hashed before it is handed out, and a blob whose bytes are not its
//! digest is refused ([`Fetch::Damaged`]), never returned and never cached. Blobs served are
//! noted, and the index is told in batches ([`Reader::flush_touches`]) so that retention
//! sees them as in use.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_pack_index::{BlobDigest, MAX_BATCH, PackIndex, PackIndexError};

use crate::blocking::offload;
use crate::cache::ReadCache;
use crate::digest::verifies;
use crate::error::ReadError;

/// The most unwanted bytes read between two blobs of a pack to make them one request: 64 KiB.
pub const DEFAULT_GAP: u64 = 64 << 10;

/// How many ranged requests of one call are in flight at once.
pub const DEFAULT_CONCURRENCY: usize = 8;

/// What a read found for one digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The blob, whose bytes were hashed and are the digest.
    Found(Bytes),
    /// The index has no row for the digest.
    Missing,
    /// The index has a row, and the store does not have the bytes it points at: the pack is
    /// gone or shorter than the row says.
    Lost,
    /// The store gave bytes that are not the digest. They are not returned.
    Damaged,
}

/// How the reader groups requests and notes use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderConfig {
    /// Two blobs of a pack that start and end at most this many bytes apart are read with one
    /// request, and the bytes between them are read and dropped.
    pub gap: u64,
    /// How many requests of one call are in flight at once. At least one is used.
    pub concurrency: usize,
    /// How many distinct blobs served are noted before the index is told: the noted blobs go
    /// to the index in a batch when this many are noted.
    pub touch_batch: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            gap: DEFAULT_GAP,
            concurrency: DEFAULT_CONCURRENCY,
            touch_batch: MAX_BATCH,
        }
    }
}

/// One ranged request: the blobs of one pack from `start` to `end`.
struct Span {
    pack: ObjectKey,
    start: u64,
    end: u64,
    /// The blobs in the range, each with its offset in the pack.
    members: Vec<(BlobDigest, u64)>,
}

/// A span with what the store answered: `None` when the store does not have the bytes.
type Fetched = (Span, Result<Option<Bytes>, ReadError>);

/// Reads blobs from the packs of an object store, through a pack index and an optional cache.
pub struct Reader<S: ObjectStore> {
    store: Arc<S>,
    index: Arc<dyn PackIndex>,
    cache: Option<Arc<ReadCache>>,
    config: ReaderConfig,
    /// The blobs served that the index has not been told of.
    pending: Mutex<BTreeSet<BlobDigest>>,
}

impl<S: ObjectStore> Reader<S> {
    /// A reader over `store` and `index`, with `cache` (when there is one) in front.
    #[must_use]
    pub fn new(
        store: Arc<S>,
        index: Arc<dyn PackIndex>,
        cache: Option<Arc<ReadCache>>,
        config: ReaderConfig,
    ) -> Self {
        let config = ReaderConfig {
            concurrency: config.concurrency.max(1),
            ..config
        };
        Self {
            store,
            index,
            cache,
            config,
            pending: Mutex::default(),
        }
    }

    /// Reads `digests`; the answer has one [`Fetch`] for each, in the order given. A digest
    /// named twice is read once.
    ///
    /// A blob that is not in the index, not in the store, or damaged is the blob's [`Fetch`],
    /// not an error of the call. The blobs read from the store are put in the cache; a cache
    /// that cannot take them does not fail the read.
    ///
    /// # Errors
    /// [`ReadError::Index`] when the index fails; [`ReadError::Key`] when the index names a
    /// pack by a string that is not an object key; [`ReadError::Store`] when the store fails a
    /// read for a reason other than a missing object. No blob is returned then.
    pub async fn read(&self, digests: &[BlobDigest]) -> Result<Vec<Fetch>, ReadError> {
        let distinct: BTreeSet<BlobDigest> = digests.iter().copied().collect();
        let wanted: Vec<BlobDigest> = distinct.into_iter().collect();
        let mut found = BTreeMap::new();
        let rest = self.answer_from_cache(wanted, &mut found).await;
        let spans = self.plan(&rest, &mut found).await?;
        let mut fresh = Vec::new();
        for (span, got) in self.fetch_all(spans).await {
            let bytes = got?;
            for (digest, offset) in span.members {
                let fetch = match &bytes {
                    None => Fetch::Lost,
                    Some(whole) => carve(&digest, whole, offset - span.start),
                };
                if let Fetch::Found(blob) = &fetch {
                    fresh.push((digest, blob.clone()));
                }
                found.insert(digest, fetch);
            }
        }
        self.fill_cache(fresh).await;
        self.note(&found).await;
        let answer = digests
            .iter()
            .map(|digest| found.get(digest).cloned().unwrap_or(Fetch::Missing));
        Ok(answer.collect())
    }

    /// Tells the index of the blobs served since the last time, in batches. Returns how many
    /// blobs the index touched. A read does this by itself once enough blobs are noted; a
    /// process calls it when it stops.
    ///
    /// # Errors
    /// [`PackIndexError`] when the index fails; the blobs of the batches not yet sent are
    /// dropped, as the next read of them notes them again.
    pub async fn flush_touches(&self) -> Result<usize, PackIndexError> {
        let taken = std::mem::take(&mut *self.pending());
        let noted: Vec<BlobDigest> = taken.into_iter().collect();
        let mut touched = 0;
        for chunk in noted.chunks(MAX_BATCH) {
            let index = Arc::clone(&self.index);
            let batch = chunk.to_vec();
            touched += offload(move || index.touch(&batch)).await?;
        }
        Ok(touched)
    }

    /// Answers from the cache what it holds, in `found`, and returns the rest of `wanted`.
    async fn answer_from_cache(
        &self,
        wanted: Vec<BlobDigest>,
        found: &mut BTreeMap<BlobDigest, Fetch>,
    ) -> Vec<BlobDigest> {
        let Some(cache) = &self.cache else {
            return wanted;
        };
        let cache = Arc::clone(cache);
        let (wanted, held) = offload(move || {
            let held: Vec<_> = wanted.iter().map(|digest| cache.get(digest)).collect();
            (wanted, held)
        })
        .await;
        let mut rest = Vec::new();
        for (digest, bytes) in wanted.into_iter().zip(held) {
            match bytes {
                Some(bytes) => {
                    found.insert(digest, Fetch::Found(bytes));
                }
                None => rest.push(digest),
            }
        }
        rest
    }

    /// Asks the index where `rest` are. The ones it does not know are [`Fetch::Missing`] in
    /// `found`; the empty ones it knows are answered there too, as they are read from no pack;
    /// the others are returned as the ranged requests that read them.
    async fn plan(
        &self,
        rest: &[BlobDigest],
        found: &mut BTreeMap<BlobDigest, Fetch>,
    ) -> Result<Vec<Span>, ReadError> {
        let mut by_pack: BTreeMap<String, Vec<(u64, BlobDigest)>> = BTreeMap::new();
        for chunk in rest.chunks(MAX_BATCH) {
            let index = Arc::clone(&self.index);
            let batch = chunk.to_vec();
            let located = offload(move || index.locate(&batch)).await?;
            for (digest, place) in chunk.iter().zip(located) {
                match place {
                    None => {
                        found.insert(*digest, Fetch::Missing);
                    }
                    Some(_) if digest.size == 0 => {
                        found.insert(*digest, finish(digest, Bytes::new()));
                    }
                    Some(place) => {
                        let members = by_pack.entry(place.pack_key).or_default();
                        members.push((place.offset, *digest));
                    }
                }
            }
        }
        let mut spans = Vec::new();
        for (key, members) in by_pack {
            let pack = ObjectKey::new(key)?;
            spans.extend(coalesce(&pack, members, self.config.gap));
        }
        Ok(spans)
    }

    /// Reads every span, `concurrency` at a time.
    async fn fetch_all(&self, spans: Vec<Span>) -> Vec<Fetched> {
        stream::iter(spans)
            .map(|span| async move {
                let got = self.fetch(&span).await;
                (span, got)
            })
            .buffer_unordered(self.config.concurrency)
            .collect::<Vec<_>>()
            .await
    }

    /// One ranged request.
    async fn fetch(&self, span: &Span) -> Result<Option<Bytes>, ReadError> {
        // A span holds a blob of at least one byte that ends inside its pack, which the index
        // checked when it recorded the pack, so the range is not empty and does not overflow.
        let range = ByteRange::new(span.start, span.end - span.start)
            .expect("a span holds a blob of at least one byte");
        match self.store.get_range(&span.pack, range).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if is_absent(&error) => Ok(None),
            Err(source) => Err(ReadError::Store {
                pack: span.pack.to_string(),
                source,
            }),
        }
    }

    /// Puts what the store gave in the cache. A cache that fails is logged and goes on.
    async fn fill_cache(&self, fresh: Vec<(BlobDigest, Bytes)>) {
        let Some(cache) = &self.cache else { return };
        let cache = Arc::clone(cache);
        let failures = offload(move || {
            let mut errors = Vec::new();
            for (digest, bytes) in &fresh {
                errors.extend(cache.put(digest, bytes).err());
            }
            errors
        })
        .await;
        for error in failures {
            tracing::warn!(%error, "could not keep a blob in the read cache");
        }
    }

    /// Notes the blobs served, and tells the index once enough are noted.
    async fn note(&self, found: &BTreeMap<BlobDigest, Fetch>) {
        let due = {
            let mut pending = self.pending();
            for (digest, fetch) in found {
                if matches!(fetch, Fetch::Found(_)) {
                    pending.insert(*digest);
                }
            }
            pending.len() >= self.config.touch_batch
        };
        if !due {
            return;
        }
        if let Err(error) = self.flush_touches().await {
            tracing::warn!(%error, "could not tell the pack index of the blobs served");
        }
    }

    fn pending(&self) -> MutexGuard<'_, BTreeSet<BlobDigest>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether the store answered that it does not have the bytes asked for, as opposed to
/// failing to answer.
fn is_absent(error: &ObjectStoreError) -> bool {
    matches!(
        error,
        ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. }
    )
}

/// The blob `digest` from `whole`, the bytes of a range, in which it starts at `from`: lost
/// when the range ends before the blob does.
fn carve(digest: &BlobDigest, whole: &Bytes, from: u64) -> Fetch {
    let lo = from as usize;
    let hi = lo.saturating_add(digest.size as usize);
    if hi > whole.len() {
        return Fetch::Lost;
    }
    // A blob that is a part of the range is copied, so that it does not keep the whole range
    // in memory for as long as the caller holds it.
    let blob = if hi - lo == whole.len() {
        whole.clone()
    } else {
        Bytes::copy_from_slice(&whole[lo..hi])
    };
    finish(digest, blob)
}

/// The blob if its bytes are the digest, else [`Fetch::Damaged`].
fn finish(digest: &BlobDigest, bytes: Bytes) -> Fetch {
    if verifies(digest, &bytes) {
        Fetch::Found(bytes)
    } else {
        Fetch::Damaged
    }
}

/// The ranged requests that read `members` (each a blob with its offset) of the pack `pack`:
/// one for each run of blobs that lie within `gap` bytes of each other.
fn coalesce(pack: &ObjectKey, mut members: Vec<(u64, BlobDigest)>, gap: u64) -> Vec<Span> {
    members.sort();
    let mut spans: Vec<Span> = Vec::new();
    for (offset, digest) in members {
        let end = offset.saturating_add(digest.size);
        match spans.last_mut() {
            Some(span) if offset <= span.end.saturating_add(gap) => {
                span.end = span.end.max(end);
                span.members.push((digest, offset));
            }
            _ => spans.push(Span {
                pack: pack.clone(),
                start: offset,
                end,
                members: vec![(digest, offset)],
            }),
        }
    }
    spans
}
