//! [`PackedCas`]: a daemon's data plane that reads blobs from packs first.
//!
//! A pack is one object in the farm's object store that holds many blobs; the frontend's pack
//! index says where each lies (`kbf.worker.v1.PackIndex`, reached through [`crate::RemoteIndex`]).
//! `PackedCas` wraps the data plane the daemon has, `C`, and for a read ([`Cas::get`] and
//! [`Cas::read_to`], which is how a lease fetches its action, its input tree and its input
//! files) asks `kbf_pack::Reader` first, which finds the blob through the index, reads its bytes
//! from the pack with a ranged request and hashes them against the digest:
//!
//! - **Found**: the blob is served from the pack.
//! - **Missing**, or the index could not be asked: the read goes to `C` exactly as it did
//!   without packs. A blob that is in no pack is read where it always was. A frontend that
//!   serves no locations (UNIMPLEMENTED or UNAVAILABLE) places no blob, so its reads are
//!   missing ones, counted in [`PackStats::elsewhere`]; any other failure of the index or of
//!   the object store is "could not be asked", counted in [`PackStats::unasked`] and logged.
//! - **Lost** (the index has a row and the object store does not have the bytes it points at) and
//!   **damaged** (the bytes are not the digest): the read fails, as [`CasError::Unavailable`] and
//!   [`CasError::Corrupt`], and is logged and counted. Nothing falls back to `C` for these: the
//!   index says the blob is in a pack, and a bad pack is for the operator to know.
//!
//! **Writes** are `C`'s, unchanged, unless the plane was built to write packs
//! ([`PackedCas::with_writes`]): then [`Cas::put`], [`Cas::put_chunks`], [`Cas::put_records`] and
//! [`Cas::put_spool`] go into packs, and [`Cas::missing`] asks the index first (the `write`
//! module docs have the write rule, the fallbacks and the counts). [`Cas::intake`] is always
//! `C`'s.
//!
//! The Cas has no call that reads many blobs, so each read asks the index about one blob: one
//! `Locate` call and one ranged request. A lease's input tree is fetched by many concurrent
//! reads, and each is its own call; batching them is a change to the trait. Each call waits for
//! the frontend's answer on a thread of the runtime's blocking pool, which the daemon's file
//! operations share, so at most `MAX_ASKING` reads ask the packs at once, whichever lease
//! they are for; the others wait for their turn.
//!
//! A blob read from a pack is held whole in memory, outside the transfer budget, so a blob of
//! more than `MAX_PACKED_READ_BYTES` (64 MiB) is not asked of the packs: it is read by `C`,
//! which streams it to a file, and is counted as read elsewhere.
//!
//! The counts of what was served and why a read went elsewhere are [`PackStats`], logged when
//! the last plane and [`PackCounter`] counting into them are dropped. A daemon given the counter
//! ([`crate::Daemon::with_pack_reads`]) sends the read totals in every heartbeat, and the write
//! totals too when it also writes packs ([`crate::Daemon::with_pack_writes`]).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_objstore::ObjectStore;
use kbf_pack::{Blob, Fetch, ReadError, Reader, blob_digest};
use kbf_pack_index::BlobDigest;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{PackReadCounts, PackWriteCounts};
use kbf_transfer::cas::{Cas, CasError, Chunks, Intake, label};
use kbf_transfer::{Record, Spool};
use kbf_types::LeaseId;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Semaphore;

pub use self::write::{
    DEFAULT_WRITE_INFLIGHT_BYTES, FallbackStats, PackCommit, PackWriter, WriteSettings, WriteStats,
};
use self::write::{Room, Stored, WriteCounts, Writes, gather, groups, load, packable};
use crate::remote_index::wire;

/// The most reads of a plane, its leases' included, that ask the packs at once. Each holds a
/// thread of the runtime's blocking pool until the frontend answers, and that pool is shared
/// with the daemon's file operations: an input tree of many files must not take all of it.
const MAX_ASKING: usize = 16;

/// The largest blob, in bytes, that is read from a pack. A read from a pack holds the whole blob
/// in memory (and `get` copies it once), outside the transfer budget
/// (`--transfer-bytes-in-flight`, `--transfer-memory-blob-bytes`), where the plane underneath
/// streams a large file to disk in chunks: a larger blob is read by the plane underneath, and
/// the packs are not asked.
const MAX_PACKED_READ_BYTES: i64 = 64 << 20;

/// What [`PackedCas`] reads packs with: blobs by digest, one [`Fetch`] each. Object safe (the
/// plane holds it behind a pointer, so it is one type whatever the object store); the provided
/// implementation is `kbf_pack::Reader`.
pub trait PackReads: Send + Sync + 'static {
    /// The blobs `digests`, as `kbf_pack::Reader::read` answers them: one [`Fetch`] each, in
    /// order.
    ///
    /// # Errors
    /// The index or the store failed the call, for a reason other than a blob that is missing,
    /// lost or damaged.
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>>;
}

impl<S: ObjectStore> PackReads for Reader<S> {
    fn read_blobs<'a>(
        &'a self,
        digests: &'a [BlobDigest],
    ) -> BoxFuture<'a, Result<Vec<Fetch>, ReadError>> {
        Box::pin(self.read(digests))
    }
}

/// What a [`PackedCas`] did, counting every plane made from it (its leases too).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackStats {
    /// Blobs served from a pack.
    pub served: u64,
    /// The bytes of those blobs.
    pub served_bytes: u64,
    /// Reads of a blob the index does not have, or that is not a SHA-256 digest, or that is too
    /// large to be read whole from a pack, which went to the plane underneath. A frontend that serves no locations (UNIMPLEMENTED or UNAVAILABLE)
    /// places no blob, so its reads are counted here too.
    pub elsewhere: u64,
    /// Reads that went to the plane underneath because the packs could not be asked: the index
    /// or the object store failed, other than by a frontend that serves no locations.
    pub unasked: u64,
    /// Reads that failed because the pack the index points at is gone or too short.
    pub lost: u64,
    /// Reads that failed because the bytes in the pack are not the digest.
    pub damaged: u64,
    /// What the plane wrote into packs, and what it wrote elsewhere instead.
    pub writes: WriteStats,
}

impl fmt::Display for PackStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "served={} served_bytes={} elsewhere={} unasked={} lost={} damaged={}",
            self.served, self.served_bytes, self.elsewhere, self.unasked, self.lost, self.damaged
        )
    }
}

impl From<PackStats> for PackReadCounts {
    fn from(stats: PackStats) -> Self {
        Self {
            served: stats.served,
            served_bytes: stats.served_bytes,
            elsewhere: stats.elsewhere,
            unasked: stats.unasked,
            lost: stats.lost,
            damaged: stats.damaged,
        }
    }
}

impl From<WriteStats> for PackWriteCounts {
    fn from(stats: WriteStats) -> Self {
        let by = stats.fallbacks;
        Self {
            packs: stats.packs,
            packed: stats.packed,
            packed_bytes: stats.packed_bytes,
            fallback_off: by.off,
            fallback_unfit: by.unfit,
            fallback_unsupported: by.unsupported,
            fallback_refused: by.refused,
            fallback_store: by.store,
            fallback_record: by.record,
            fallback_index: by.index,
            missing_indexed: stats.missing_indexed,
            missing_unasked: stats.missing_unasked,
        }
    }
}

/// A handle on the counts of the [`PackedCas`] made with it ([`PackedCas::with_counter`]), and
/// of every plane made from that one. The daemon holds one to put the counts in its heartbeats
/// ([`crate::Daemon::with_pack_reads`]); a clone counts into the same totals.
#[derive(Clone, Debug, Default)]
pub struct PackCounter(Arc<Counts>);

impl PackCounter {
    /// What the planes counting into this handle have done so far.
    #[must_use]
    pub fn stats(&self) -> PackStats {
        self.0.stats()
    }
}

/// [`PackStats`] as they are counted; logged when the last handle on them goes.
#[derive(Debug, Default)]
struct Counts {
    served: AtomicU64,
    served_bytes: AtomicU64,
    elsewhere: AtomicU64,
    unasked: AtomicU64,
    lost: AtomicU64,
    damaged: AtomicU64,
    /// Whether the last read that asked the packs could not (and it was logged as a warning).
    failing: AtomicBool,
    writes: WriteCounts,
}

impl Counts {
    fn stats(&self) -> PackStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        PackStats {
            served: get(&self.served),
            served_bytes: get(&self.served_bytes),
            elsewhere: get(&self.elsewhere),
            unasked: get(&self.unasked),
            lost: get(&self.lost),
            damaged: get(&self.damaged),
            writes: self.writes.stats(),
        }
    }
}

impl Drop for Counts {
    fn drop(&mut self) {
        let stats = self.stats();
        tracing::info!("pack reads: {stats}");
        tracing::info!("pack writes: {}", stats.writes);
    }
}

/// Adds `n` to `counter`.
fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// What the packs said about one read.
enum Found {
    /// The blob, hashed.
    Blob(Bytes),
    /// The read fails with this: the blob is in a pack that is gone or damaged.
    Refused(CasError),
    /// Not for the packs: the plane underneath reads it.
    Elsewhere,
}

/// A data plane that reads blobs from packs first (the module docs).
pub struct PackedCas<C: Cas> {
    inner: Arc<C>,
    packs: Arc<dyn PackReads>,
    counts: Arc<Counts>,
    asking: Arc<Semaphore>,
    /// What this plane writes packs with; `None` for a plane that only reads them.
    writes: Option<Writes>,
}

impl<C: Cas> PackedCas<C> {
    /// `inner`, with its reads going to `packs` first.
    #[must_use]
    pub fn new(inner: Arc<C>, packs: Arc<dyn PackReads>) -> Self {
        Self::with_counter(inner, packs, &PackCounter::default())
    }

    /// [`Self::new`], counting into `counter`, which the caller keeps to read the counts
    /// without the plane.
    #[must_use]
    pub fn with_counter(inner: Arc<C>, packs: Arc<dyn PackReads>, counter: &PackCounter) -> Self {
        Self {
            inner,
            packs,
            counts: Arc::clone(&counter.0),
            asking: Arc::new(Semaphore::new(MAX_ASKING)),
            writes: None,
        }
    }

    /// This plane, its outputs written into packs by `writer` while `commit` is on (the `write`
    /// module docs). `commit` is the handle the daemon sets from each `Welcome`
    /// ([`crate::Daemon::with_pack_writes`]); a plane given a handle nothing turns on writes no
    /// pack.
    #[must_use]
    pub fn with_writes(mut self, writer: Arc<PackWriter>, commit: PackCommit) -> Self {
        self.writes = Some(Writes::new(writer, commit));
        self
    }

    /// What this plane, and every plane made from it, has done so far.
    #[must_use]
    pub fn stats(&self) -> PackStats {
        self.counts.stats()
    }

    /// Logs that the packs could not be asked for `digest`: a warning for the first read after
    /// one that was answered, and only a debug line for each read after it, as a frontend that
    /// cannot be asked (one of the previous release answers PERMISSION_DENIED) fails them all.
    fn say_unasked(&self, digest: &Digest, error: &ReadError) {
        let repeated = self.counts.failing.swap(true, Ordering::Relaxed);
        if repeated {
            tracing::debug!(
                blob = %label(digest),
                %error,
                "pack reads: the packs could not be asked"
            );
        } else {
            tracing::warn!(
                blob = %label(digest),
                %error,
                "pack reads: the packs could not be asked; reading the blobs as before"
            );
        }
    }

    /// What writes `blobs` packs: the writer, if the plane has one and the frontend takes
    /// commits. A plane that has one and may not use it counts the `blobs` as written
    /// elsewhere, because the capability is off.
    fn writing(&self, blobs: usize) -> Option<&Writes> {
        let writes = self.writes.as_ref()?;
        if writes.enabled() {
            return Some(writes);
        }
        self.counts.writes.off(blobs);
        None
    }

    /// The room for the blob `digest` names, if it is a blob a pack takes and the budget can
    /// hold it; otherwise the blob is counted as written elsewhere, because it does not fit.
    async fn blob_room(
        &self,
        writes: &Writes,
        digest: Option<BlobDigest>,
    ) -> Option<(BlobDigest, Room)> {
        let found = match digest.filter(|digest| packable(digest.size)) {
            Some(digest) => writes.room(digest.size).await.map(|room| (digest, room)),
            None => None,
        };
        if found.is_none() {
            self.counts.writes.unfit(1);
        }
        found
    }

    /// `bytes` of room for `blobs` blobs; if the budget cannot hold them, they are counted as
    /// written elsewhere, because they do not fit.
    async fn room(&self, writes: &Writes, bytes: u64, blobs: usize) -> Option<Room> {
        let room = writes.room(bytes).await;
        if room.is_none() {
            self.counts.writes.unfit(blobs);
        }
        room
    }

    /// Whether `blobs` are now in committed packs (`room` counts their bytes): `false` when
    /// the plane underneath is to write them.
    ///
    /// # Errors
    /// [`CasError::Corrupt`] when a blob's bytes are not its digest.
    async fn packed(
        &self,
        writes: &Writes,
        blobs: Vec<Blob>,
        room: Room,
    ) -> Result<bool, CasError> {
        match writes.store(blobs, room, &self.counts.writes).await {
            Stored::Packed => Ok(true),
            Stored::Elsewhere => Ok(false),
            Stored::Refused(error) => Err(error),
        }
    }

    /// What the packs have for `digest`.
    async fn find(&self, digest: &Digest) -> Found {
        if digest.size_bytes > MAX_PACKED_READ_BYTES {
            add(&self.counts.elsewhere, 1);
            return Found::Elsewhere;
        }
        let Some(blob) = blob_of(digest) else {
            add(&self.counts.elsewhere, 1);
            return Found::Elsewhere;
        };
        let turn = self.asking.acquire().await;
        let read = self.packs.read_blobs(std::slice::from_ref(&blob)).await;
        drop(turn);
        let fetch = match read {
            Ok(mut fetched) => {
                self.counts.failing.store(false, Ordering::Relaxed);
                fetched.pop().unwrap_or(Fetch::Missing)
            }
            Err(error) => {
                add(&self.counts.unasked, 1);
                self.say_unasked(digest, &error);
                return Found::Elsewhere;
            }
        };
        match fetch {
            Fetch::Found(bytes) => {
                add(&self.counts.served, 1);
                add(&self.counts.served_bytes, bytes.len() as u64);
                Found::Blob(bytes)
            }
            Fetch::Missing => {
                add(&self.counts.elsewhere, 1);
                Found::Elsewhere
            }
            Fetch::Lost => {
                add(&self.counts.lost, 1);
                tracing::warn!(
                    blob = %label(digest),
                    "pack reads: the pack that holds the blob is gone or too short"
                );
                let why = "the pack that holds it is gone or shorter than the index says";
                Found::Refused(CasError::Unavailable(label(digest), why.to_owned()))
            }
            Fetch::Damaged => {
                add(&self.counts.damaged, 1);
                tracing::error!(
                    blob = %label(digest),
                    "pack reads: the bytes of the blob in its pack are not its digest"
                );
                let other = "another digest, read from its pack";
                Found::Refused(CasError::Corrupt(label(digest), other.to_owned()))
            }
        }
    }
}

/// The index's name for `digest`, if it is a SHA-256 digest.
fn blob_of(digest: &Digest) -> Option<BlobDigest> {
    let bytes = hex::decode(&digest.hash).ok()?;
    let hash = <[u8; 32]>::try_from(bytes).ok()?;
    let size = u64::try_from(digest.size_bytes).ok()?;
    Some(BlobDigest { hash, size })
}

/// Writes `bytes` into `file` and flushes it. A write that failed is the answer, the flush that
/// follows it notwithstanding.
async fn write_into(file: &mut tokio::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    let written = file.write_all(bytes).await;
    let flushed = file.flush().await;
    written.and(flushed)
}

impl<C: Cas> Cas for PackedCas<C> {
    type Lease = PackedCas<C::Lease>;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> PackedCas<C::Lease> {
        PackedCas {
            inner: Arc::new(C::lease(&self.inner, lease)),
            packs: Arc::clone(&self.packs),
            counts: Arc::clone(&self.counts),
            asking: Arc::clone(&self.asking),
            writes: self.writes.clone(),
        }
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        match self.find(digest).await {
            Found::Blob(bytes) => Ok(bytes.to_vec()),
            Found::Refused(error) => Err(error),
            Found::Elsewhere => C::get(&self.inner, digest).await,
        }
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        match self.find(digest).await {
            Found::Blob(bytes) => write_into(file, &bytes)
                .await
                .map_err(|e| CasError::Write(label(digest), e.to_string())),
            Found::Refused(error) => Err(error),
            Found::Elsewhere => C::read_to(&self.inner, digest, file).await,
        }
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let Some(writes) = self.writing(1) else {
            return C::put(&self.inner, bytes).await;
        };
        let found = self.blob_room(writes, Some(blob_digest(&bytes))).await;
        let Some((digest, room)) = found else {
            return C::put(&self.inner, bytes).await;
        };
        let blob = Blob {
            digest,
            bytes: Bytes::from(bytes),
        };
        if self.packed(writes, vec![blob.clone()], room).await? {
            return Ok(wire(&digest));
        }
        C::put(&self.inner, blob.bytes.to_vec()).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        let Some(writes) = self.writing(1) else {
            return C::put_chunks(&self.inner, digest, chunks).await;
        };
        let found = self.blob_room(writes, blob_of(&digest)).await;
        let Some((wanted, room)) = found else {
            return C::put_chunks(&self.inner, digest, chunks).await;
        };
        let bytes = gather(&digest, chunks).await?;
        // The packer hashes a blob only when it packs it, not when the index has it already or
        // cannot be asked, and a put that falls back must not return a digest it was not given.
        if blob_digest(&bytes) != wanted {
            return Err(CasError::Corrupt(label(&digest), "other bytes".to_owned()));
        }
        let blob = Blob {
            digest: wanted,
            bytes: Bytes::from(bytes),
        };
        if self.packed(writes, vec![blob.clone()], room).await? {
            return Ok(digest);
        }
        C::put(&self.inner, blob.bytes.to_vec()).await
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        let Some(writes) = self.writes.as_ref().filter(|writes| writes.enabled()) else {
            return C::missing(&self.inner, digests).await;
        };
        let asked: Vec<BlobDigest> = digests.iter().filter_map(blob_of).collect();
        let count = asked.len();
        let answer = writes.lacking(asked).await;
        self.counts.writes.answered(count, digests.len(), &answer);
        let lacking: Vec<Digest> = match &answer {
            // The index has the blobs it did not list; the plane underneath is asked about the
            // rest, and about the digests the index does not know (not SHA-256).
            Ok(lacking) => digests
                .iter()
                .filter(|digest| blob_of(digest).is_none_or(|blob| lacking.contains(&blob)))
                .cloned()
                .collect(),
            Err(_) => digests.to_vec(),
        };
        if lacking.is_empty() {
            return lacking;
        }
        C::missing(&self.inner, &lacking).await
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        let Some(writes) = self.writing(records.len()) else {
            return C::put_records(&self.inner, records).await;
        };
        let (large, small): (Vec<Record>, Vec<Record>) = records
            .into_iter()
            .partition(|record| !packable(record.digest.size_bytes));
        self.counts.writes.unfit(large.len());
        for group in groups(small, writes.group_bytes()) {
            let bytes = group.iter().map(|record| record.digest.size_bytes).sum();
            let Some(room) = self.room(writes, bytes, group.len()).await else {
                C::put_records(&self.inner, group).await?;
                continue;
            };
            let blobs = load(&group).await?;
            if !self.packed(writes, blobs, room).await? {
                C::put_records(&self.inner, group).await?;
            }
        }
        if large.is_empty() {
            return Ok(());
        }
        C::put_records(&self.inner, large).await
    }

    async fn put_spool(&self, spool: Arc<Spool>) -> Result<(), CasError> {
        if self.writing(spool.records().len()).is_none() {
            return C::put_spool(&self.inner, spool).await;
        }
        self.put_records(spool.outgoing()).await
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        C::intake(&self.inner)
    }

    fn in_flight(&self) -> usize {
        C::in_flight(&self.inner)
    }
}

mod write;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod write_tests;
