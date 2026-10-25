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
//! **Everything else is `C`'s, unchanged**: [`Cas::missing`], [`Cas::put`], [`Cas::put_chunks`],
//! [`Cas::put_records`], [`Cas::put_spool`] and [`Cas::intake`]. `missing` is not answered from
//! the index: a daemon's outputs go to the storage members as they did, and a blob the index
//! holds is not thereby held where the closure check looks (the check is the frontend's).
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
//! the plane is dropped.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_objstore::ObjectStore;
use kbf_pack::{Fetch, ReadError, Reader};
use kbf_proto::reapi::Digest;
use kbf_state::BlobDigest;
use kbf_transfer::cas::{Cas, CasError, Chunks, Intake, label};
use kbf_transfer::{Record, Spool};
use kbf_types::LeaseId;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Semaphore;

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
}

impl fmt::Display for PackStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "served={} served_bytes={} elsewhere={} unasked={} lost={} damaged={}",
            self.served,
            self.served_bytes,
            self.elsewhere,
            self.unasked,
            self.lost,
            self.damaged
        )
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
        }
    }
}

impl Drop for Counts {
    fn drop(&mut self) {
        let stats = self.stats();
        tracing::info!("pack reads: {stats}");
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
}

impl<C: Cas> PackedCas<C> {
    /// `inner`, with its reads going to `packs` first.
    #[must_use]
    pub fn new(inner: Arc<C>, packs: Arc<dyn PackReads>) -> Self {
        Self {
            inner,
            packs,
            counts: Arc::default(),
            asking: Arc::new(Semaphore::new(MAX_ASKING)),
        }
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
        C::put(&self.inner, bytes).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        C::put_chunks(&self.inner, digest, chunks).await
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        C::missing(&self.inner, digests).await
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        C::put_records(&self.inner, records).await
    }

    async fn put_spool(&self, spool: Arc<Spool>) -> Result<(), CasError> {
        C::put_spool(&self.inner, spool).await
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        C::intake(&self.inner)
    }

    fn in_flight(&self) -> usize {
        C::in_flight(&self.inner)
    }
}

#[cfg(test)]
mod tests;
