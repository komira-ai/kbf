//! [`Packs`]: the packs of one frontend over one object store and one pack index.
//!
//! Everything that is a decision lives in [`Packs`], which is not generic: what the index
//! lacks, how a failure is answered, what is counted and logged. [`PackStore`] is the part
//! that depends on the object store's type, and it only hands calls to `kbf-pack`.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_objstore::{BodyStream, ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_pack::{
    Blob, FRONTEND_PACK_BYTES, Fetch, PackError, PackKeys, Packed, Packer, PackerConfig, ReadCache,
    ReadError, Reader, ReaderConfig, check_create_only,
};
use kbf_pack_index::{BlobDigest, Location, MAX_BATCH, PackIndex, PackIndexError};
use kbf_types::{Digest, DigestFunction};
use tonic::Status;

use super::metrics::PackMetrics;
use super::room::{Budget, DEFAULT_INFLIGHT_BYTES, Room};
use super::verifying::VerifyingKeys;
use crate::large_backfill::LargeStore;

/// The largest blob a frontend puts in a pack or serves from one by default: 64 MiB, the
/// most it holds in memory to hash a blob before the first byte of it is sent.
pub const MAX_PACKED_BLOB_BYTES: u64 = 64 << 20;

/// How long an upload into packs may go without a byte of its body arriving, by default.
pub const DEFAULT_UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest idle timeout [`Packs`] keeps: a day. A longer one is taken as a day, so that the
/// deadline of an upload (the time now and the timeout) never overflows the clock.
pub const MAX_UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// How a frontend cuts its packs, how much it takes in at once and what it keeps of what it
/// read.
#[derive(Clone)]
pub struct PackSettings {
    /// The length at which a pack of client uploads is closed.
    pub pack_bytes: u64,
    /// The largest blob that is packed or served from a pack ([`Packs::max_blob_bytes`]).
    pub max_blob_bytes: u64,
    /// The bytes that the uploads into packs may hold in flight together: each takes the size
    /// of its blob before it reads a byte, and an upload that finds the rest spent waits.
    /// Reads are not counted. An upload of a blob larger than this is refused. At most
    /// `u32::MAX`.
    pub inflight_bytes: u32,
    /// How long a ByteStream upload may go without a byte of its body arriving before it is
    /// ended DEADLINE_EXCEEDED and gives its bytes back. Only bytes count as progress: a
    /// slow upload that keeps sending is never cut. At most [`MAX_UPLOAD_IDLE_TIMEOUT`].
    pub upload_idle_timeout: Duration,
    /// The blobs read from packs are kept here, when there is a cache.
    pub cache: Option<Arc<ReadCache>>,
}

impl Default for PackSettings {
    fn default() -> Self {
        Self {
            pack_bytes: FRONTEND_PACK_BYTES,
            max_blob_bytes: MAX_PACKED_BLOB_BYTES,
            inflight_bytes: DEFAULT_INFLIGHT_BYTES,
            upload_idle_timeout: DEFAULT_UPLOAD_IDLE_TIMEOUT,
            cache: None,
        }
    }
}

/// What [`Packs`] needs of an object store: the calls of `kbf-pack` over it. Nothing here
/// decides anything.
pub trait PackStore: Send + Sync {
    /// Packs `blobs` and records the packs (the write rule: stored, then recorded).
    fn pack(&self, blobs: Vec<Blob>) -> BoxFuture<'_, Result<Packed, PackError>>;

    /// Reads `digests` through the index and the packs, one [`Fetch`] each.
    fn read<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>>;

    /// Checks that the store refuses to replace an object.
    fn check_create_only(&self) -> BoxFuture<'_, Result<(), PackError>>;

    /// The bytes of `range` of the object `key` that the store has, as the store answers.
    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        range: ByteRange,
    ) -> BoxFuture<'a, Result<Bytes, ObjectStoreError>>;

    /// The length of the object `key`, or `None` if the store has none ([`ObjectStore::len`]).
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>>;

    /// Writes the object `key` of exactly `len` bytes from `body` as it is sent
    /// ([`ObjectStore::put_stream`]): create-only, and an `Err` piece or a body of another length
    /// leaves nothing under `key`.
    fn put_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>>;
}

/// A packer and a reader over one store.
struct Over<S: ObjectStore> {
    store: Arc<S>,
    keys: PackKeys,
    packer: Packer<S>,
    reader: Reader<S>,
}

impl<S: ObjectStore> PackStore for Over<S> {
    fn pack(&self, blobs: Vec<Blob>) -> BoxFuture<'_, Result<Packed, PackError>> {
        Box::pin(self.packer.pack(blobs))
    }

    fn read<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        Box::pin(self.reader.read(digests))
    }

    fn check_create_only(&self) -> BoxFuture<'_, Result<(), PackError>> {
        Box::pin(check_create_only(&*self.store, &self.keys))
    }

    fn read_range<'a>(
        &'a self,
        key: &'a ObjectKey,
        range: ByteRange,
    ) -> BoxFuture<'a, Result<Bytes, ObjectStoreError>> {
        Box::pin(self.store.get_range(key, range))
    }

    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        Box::pin(self.store.len(key))
    }

    fn put_stream<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(self.store.put_stream(key, len, body))
    }
}

/// The clock that dates a task key's age.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The frontend's packs. Cheap to clone; every clone is the same packs.
#[derive(Clone)]
pub struct Packs {
    pub(super) store: Arc<dyn PackStore>,
    pub(super) index: Arc<dyn PackIndex>,
    max_blob_bytes: u64,
    upload_idle_timeout: Duration,
    budget: Budget,
    pub(super) metrics: Arc<PackMetrics>,
    /// What the packs' object store has been asked ([`Packs::with_store_metrics`]).
    #[allow(dead_code)]
    store_metrics: Arc<crate::store_metrics::StoreMetrics>,
    pub(super) verifying: Arc<VerifyingKeys>,
    clock: Clock,
}

impl fmt::Debug for Packs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Packs")
            .field("max_blob_bytes", &self.max_blob_bytes)
            .finish_non_exhaustive()
    }
}

impl Packs {
    /// Packs written to `store` under `keys` and recorded in `index`, as `settings` say.
    ///
    /// # Errors
    /// [`PackError::NotCreateOnly`] when the store does not claim create-only writes. Call
    /// [`Packs::check_create_only`] at startup to check the claim.
    pub fn new<S: ObjectStore>(
        store: Arc<S>,
        index: Arc<dyn PackIndex>,
        keys: PackKeys,
        settings: PackSettings,
    ) -> Result<Self, PackError> {
        let config = PackerConfig {
            pack_bytes: settings.pack_bytes,
            ..PackerConfig::default()
        };
        let packer = Packer::new(store.clone(), index.clone(), keys.clone(), config)?;
        let reader = Reader::new(
            Arc::clone(&store),
            Arc::clone(&index),
            settings.cache,
            ReaderConfig::default(),
        );
        let over = Over {
            store,
            keys,
            packer,
            reader,
        };
        let metrics = Arc::<PackMetrics>::default();
        Ok(Self {
            store: Arc::new(over),
            index,
            max_blob_bytes: settings.max_blob_bytes,
            upload_idle_timeout: settings.upload_idle_timeout.min(MAX_UPLOAD_IDLE_TIMEOUT),
            budget: Budget::new(settings.inflight_bytes, Arc::clone(&metrics)),
            metrics,
            store_metrics: Arc::new(crate::store_metrics::StoreMetrics::packs()),
            verifying: Arc::default(),
            clock: Arc::new(SystemTime::now),
        })
    }

    /// These packs rendering `metrics` as the packs' store's families (`kbf_pack_store_*`).
    /// Give the store the packs are made over the same handle through
    /// [`Counted`](crate::store_metrics::Counted); packs whose store is not counted render
    /// the families with zeros.
    #[must_use]
    pub fn with_store_metrics(mut self, metrics: Arc<crate::store_metrics::StoreMetrics>) -> Self {
        self.store_metrics = metrics;
        self
    }

    /// These packs judging the age of a task key by `clock` (the wall clock unless told).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The time these packs judge the age of a task key by.
    #[must_use]
    pub fn now(&self) -> SystemTime {
        (self.clock)()
    }

    /// The task pack keys an ingest through these packs is verifying now: the janitor of
    /// `cas/b/` never deletes one. Shared by every clone.
    #[must_use]
    pub fn verifying_keys(&self) -> Arc<VerifyingKeys> {
        Arc::clone(&self.verifying)
    }

    /// The largest blob, in bytes, that is packed or served from a pack. A larger one is
    /// stored on and read from the storage nodes, as when there are no packs.
    #[must_use]
    pub fn max_blob_bytes(&self) -> u64 {
        self.max_blob_bytes
    }

    /// The bytes that uploads into packs may hold in flight together
    /// ([`PackSettings::inflight_bytes`]).
    #[must_use]
    pub fn inflight_bytes(&self) -> u32 {
        self.budget.cap()
    }

    /// How long a ByteStream upload may go without a byte of its body arriving
    /// ([`PackSettings::upload_idle_timeout`]).
    #[must_use]
    pub fn upload_idle_timeout(&self) -> Duration {
        self.upload_idle_timeout
    }

    /// `bytes` of the room that uploads share ([`PackSettings::inflight_bytes`]), waiting for
    /// the uploads ahead to give enough back. The room is given back when it is dropped.
    ///
    /// # Errors
    /// RESOURCE_EXHAUSTED when `bytes` is over all of the room.
    pub(super) async fn reserve(&self, bytes: u64) -> Result<Room, Status> {
        self.budget.reserve(bytes).await
    }

    /// What these packs have counted.
    #[must_use]
    pub fn metrics(&self) -> &PackMetrics {
        &self.metrics
    }

    /// Appends the pack families to `out` in the Prometheus text format.
    pub fn render_metrics(&self, out: &mut String) {
        self.metrics.render(out);
    }

    /// Appends the pack families of `packs`, if a frontend has them, to `out`; without packs
    /// it appends nothing. A frontend's scrape calls this rather than matching the option
    /// itself, so that the choice is one branch of one function.
    pub fn render_metrics_of(packs: Option<&Packs>, out: &mut String) {
        if let Some(packs) = packs {
            packs.render_metrics(out);
        }
    }

    /// Checks that the object store refuses to replace an object, as the write rule needs:
    /// two writes of one key, the second of which must be refused. Run it once when the
    /// frontend starts and do not serve if it fails.
    ///
    /// # Errors
    /// Why the check failed, as text.
    pub async fn check_create_only(&self) -> Result<(), String> {
        let checked = self.store.check_create_only().await;
        checked.map_err(|error| error.to_string())
    }

    /// The digests among `asked` that the index has no row for, in the order asked.
    ///
    /// # Errors
    /// UNAVAILABLE when the index fails.
    pub async fn missing(&self, asked: &[Digest]) -> Result<Vec<Digest>, Status> {
        let mut absent = Vec::new();
        for chunk in asked.chunks(MAX_BATCH) {
            let wanted: Vec<BlobDigest> = chunk.iter().map(to_index).collect();
            let index = Arc::clone(&self.index);
            let lacking = offload(move || index.find_missing(&wanted)).await;
            absent.extend(lacking.map_err(index_failed)?.iter().map(from_index));
        }
        self.metrics.asked.add(asked.len() as u64);
        self.metrics.found.add((asked.len() - absent.len()) as u64);
        Ok(absent)
    }

    /// Where the blob of each of `asked` lies, in the order asked: `None` for one with no row in
    /// the index, or over [`Packs::max_blob_bytes`] (never packed). The index is asked in calls
    /// of at most [`MAX_BATCH`]; nothing is read from a pack and nothing is written.
    ///
    /// # Errors
    /// UNAVAILABLE when the index fails.
    pub async fn locate(&self, asked: &[Digest]) -> Result<Vec<Option<Location>>, Status> {
        let inside: Vec<BlobDigest> = asked
            .iter()
            .filter(|d| self.fits(d))
            .map(to_index)
            .collect();
        let mut found = Vec::with_capacity(inside.len());
        for chunk in inside.chunks(MAX_BATCH) {
            let wanted = chunk.to_vec();
            let index = Arc::clone(&self.index);
            let located = offload(move || index.locate(&wanted)).await;
            found.extend(located.map_err(index_failed)?);
        }
        let mut found = found.into_iter();
        Ok(asked
            .iter()
            .map(|digest| {
                if self.fits(digest) {
                    found.next().flatten()
                } else {
                    None
                }
            })
            .collect())
    }

    /// The length of the object `key` in the pack store (a HEAD), `None` if there is none.
    ///
    /// # Errors
    /// UNAVAILABLE when the store fails or `key` is not an object key.
    pub async fn object_len(&self, key: &str) -> Result<Option<u64>, Status> {
        let failed = |e: &dyn fmt::Display| Status::unavailable(format!("object store: {e}"));
        let key = ObjectKey::new(key).map_err(|e| failed(&e))?;
        self.store.len(&key).await.map_err(|e| failed(&e))
    }

    /// Writes `blobs` into packs and records them. They are in the pack index, and the packs
    /// are stored, when this returns `Ok`. When it fails, the packs finished before the failure
    /// stay recorded (each pack is recorded before the next is started), so a call repeated
    /// packs only the rest; a pack that was stored and not recorded is an object no row points
    /// at.
    ///
    /// The bytes of `blobs` are taken from the room that uploads share first, waiting for it
    /// if the uploads ahead hold it all, and given back when this returns.
    ///
    /// # Errors
    /// INVALID_ARGUMENT when a blob's bytes are not its digest; UNAVAILABLE when the store
    /// or the index failed; RESOURCE_EXHAUSTED when the blobs together are over all of the
    /// room.
    pub async fn put(&self, blobs: Vec<(Digest, Bytes)>) -> Result<(), Status> {
        let bytes = blobs.iter().map(|(_, data)| data.len() as u64).sum();
        let room = self.reserve(bytes).await?;
        self.put_in(blobs, room).await
    }

    /// As [`Packs::put`], for blobs whose bytes are already counted in `room`, which is
    /// given back when this returns.
    ///
    /// # Errors
    /// As [`Packs::put`], but for RESOURCE_EXHAUSTED.
    pub(super) async fn put_in(
        &self,
        blobs: Vec<(Digest, Bytes)>,
        room: Room,
    ) -> Result<(), Status> {
        let blobs = blobs
            .into_iter()
            .map(|(digest, bytes)| Blob {
                digest: to_index(&digest),
                bytes,
            })
            .collect();
        let packed = self.store.pack(blobs).await;
        drop(room);
        self.stored(packed)
    }

    /// The blob `digest` from its pack, hashed; `None` when the packs do not serve it and
    /// the storage nodes are to be asked: the index has no row, the row's pack is gone, or
    /// the blob is over [`Packs::max_blob_bytes`].
    ///
    /// # Errors
    /// UNAVAILABLE when the index or the store failed, or the pack held other bytes than the
    /// digest (never served).
    pub async fn read(&self, digest: &Digest) -> Result<Option<Bytes>, Status> {
        let mut answers = self.read_many(std::slice::from_ref(digest)).await;
        // One digest was asked: one answer comes back.
        answers.swap_remove(0)
    }

    /// The blobs `digests` from their packs, one answer each in the order asked, as
    /// [`Packs::read`] answers. The blobs within [`Packs::max_blob_bytes`] are read together,
    /// so that the index is asked once for the batch and blobs that lie close together in a
    /// pack are one request; a larger blob is `Ok(None)` and is not asked for. When the index
    /// or the store fails, each blob that was asked for is that failure.
    pub async fn read_many(&self, digests: &[Digest]) -> Vec<Result<Option<Bytes>, Status>> {
        let inside: Vec<&Digest> = digests.iter().filter(|d| self.fits(d)).collect();
        let asked: Vec<BlobDigest> = inside.iter().copied().map(to_index).collect();
        let read = self.store.read(&asked).await;
        let mut served = self.served(&inside, read).into_iter();
        digests
            .iter()
            .map(|digest| {
                if self.fits(digest) {
                    served.next().unwrap_or(Ok(None))
                } else {
                    Ok(None)
                }
            })
            .collect()
    }

    /// Whether a blob of this digest is within [`Packs::max_blob_bytes`].
    fn fits(&self, digest: &Digest) -> bool {
        digest.size_bytes <= self.max_blob_bytes
    }

    /// The answer to a write, and its counts.
    fn stored(&self, packed: Result<Packed, PackError>) -> Result<(), Status> {
        match packed {
            Ok(packed) => {
                for pack in &packed.packs {
                    self.metrics.writes.add(1);
                    self.metrics.written_bytes.add(pack.size);
                    self.metrics.written_blobs.add(pack.blobs as u64);
                    tracing::debug!(
                        key = %pack.key,
                        size = pack.size,
                        blobs = pack.blobs,
                        "pack written"
                    );
                }
                self.metrics.skipped_blobs.add(packed.skipped as u64);
                Ok(())
            }
            Err(PackError::DigestMismatch { claimed }) => {
                let digest = from_index(&claimed);
                let message = format!("the bytes of a blob do not hash to its digest {digest}");
                Err(Status::invalid_argument(message))
            }
            Err(error) => {
                self.metrics.write_failures.add(1);
                tracing::warn!(%error, "blobs were not stored in packs");
                let message = format!("storing blobs in packs: {error}");
                Err(Status::unavailable(message))
            }
        }
    }

    /// The answers to a read of `digests`, one each in order, and their counts.
    fn served(
        &self,
        digests: &[&Digest],
        read: Result<Vec<Fetch>, ReadError>,
    ) -> Vec<Result<Option<Bytes>, Status>> {
        match read {
            Ok(fetched) => digests
                .iter()
                .zip(fetched)
                .map(|(digest, fetch)| self.fetched(digest, fetch))
                .collect(),
            Err(error) => digests
                .iter()
                .map(|digest| Err(self.failed(digest, &error)))
                .collect(),
        }
    }

    /// The answer to a read of `digest` that found `fetch`, and its counts.
    fn fetched(&self, digest: &Digest, fetch: Fetch) -> Result<Option<Bytes>, Status> {
        match fetch {
            Fetch::Found(bytes) => {
                self.metrics.reads.add(1);
                self.metrics.read_bytes.add(bytes.len() as u64);
                Ok(Some(bytes))
            }
            Fetch::Missing => {
                self.metrics.fallbacks.add(1);
                tracing::debug!(%digest, "not in a pack: asking the storage nodes");
                Ok(None)
            }
            Fetch::Lost => {
                self.metrics.lost.add(1);
                tracing::warn!(
                    %digest,
                    "the index has the blob and its pack does not: asking the storage nodes"
                );
                Ok(None)
            }
            Fetch::Damaged => {
                self.metrics.digest_mismatches.add(1);
                tracing::error!(
                    %digest,
                    "a pack holds other bytes than the digest names: not served"
                );
                let message = format!("blob {digest} is in a pack as other bytes than its digest");
                Err(Status::unavailable(message))
            }
        }
    }

    /// The failure of a read of `digest`, and its count.
    fn failed(&self, digest: &Digest, error: &ReadError) -> Status {
        self.metrics.read_errors.add(1);
        tracing::warn!(%digest, %error, "a pack could not be read");
        Status::unavailable(format!("reading blob {digest} from packs: {error}"))
    }
}

/// The index's form of a digest.
fn to_index(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// The digest an index row names.
pub(super) fn from_index(digest: &BlobDigest) -> Digest {
    Digest::new(DigestFunction::Sha256, digest.hash, digest.size)
}

/// UNAVAILABLE for an index that could not say which blobs it has, and a warning that it
/// could not.
fn index_failed(error: PackIndexError) -> Status {
    tracing::warn!(%error, "the pack index could not say which blobs it has");
    Status::unavailable(format!("pack index: {error}"))
}

/// Runs `work`, which blocks (a call of the pack index), on the runtime's blocking pool and
/// waits for it.
///
/// # Panics
/// If `work` panics, or the runtime shuts down under it: neither is an error to hand back.
pub(super) async fn offload<T, F>(work: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(work).await;
    joined.expect("a blocking pack index call panicked")
}

impl LargeStore for Packs {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        self.store.len(key)
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        self.store.put_stream(key, len, body)
    }
}
