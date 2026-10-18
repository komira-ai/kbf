//! Records written onto the write tier as their bytes arrive: a [`TierRange`] (a block of
//! its own) for a segment upload, and the write pool ([`super::pool`]) for a streamed
//! record: its blob appended to a segment shared with others by one of a fixed number of
//! writers, then synced and committed with one `PutStaged`.
//!
//! Two callers write through one:
//!
//! - **A segment upload on a write tier** (`crate::segments`): `Begin` reserves a range
//!   for each segment's records ([`Cache::reserve_range`]), each `PutRange` writes its
//!   bytes at their offset, and `Commit` stages the range with the records the footer
//!   declared ([`Cache::stage_range`]). What the upload holds in memory is the range
//!   being written, not the segment.
//! - **A record streamed by a daemon in the server's own process** (`kbf all-in-one`'s
//!   plane, and the storage node), whose digest is known only at its end:
//!   [`Cache::stream`] opens a place in the pool; [`RecordStream::append`] hands each
//!   chunk on as the caller reads it (held in memory for a small blob, written by a
//!   writer as it arrives for a larger one), and keeps the CRC-32C of what it handed
//!   on; [`RecordStream::finish`] checks that the
//!   bytes written are as many as the digest names and that their CRC-32C is the
//!   caller's; [`StreamedRecord::commit`] syncs and stages it (in one
//!   sync with any blobs queued beside it).
//!
//! Either way the digests are trusted at the commit, as a segment upload's are with a
//! tier: the flusher hashes every record before its PUT and drops one that does not
//! match. Without a write tier, or when the tier has no room, there is no range: a
//! segment upload holds the bytes in memory (records of at most 16 MiB), and a
//! [`RecordStream`] goes straight to the object store instead (`stream_store`), its
//! bytes hashed as they pass and checked against the digest claimed at the commit.

use std::fmt;
use std::sync::Arc;

use futures::StreamExt as _;
use kbf_objstore::{BodyStream, ObjectStore};
use kbf_segments::{Crc32c, RecordHasher};
use kbf_tier::{Reservation, TierError};

use kbf_types::Digest;

use super::pool::{PoolCommit, PoolWrite, Refused};
use super::stream_store::{Claim, ToStore};
use super::tier::{Staging, blocking};
use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

/// A block of its own on the write tier, reserved for records written as they arrive.
/// Dropped before it is staged, it is answered as failed and nothing is stored.
pub(crate) struct TierRange {
    staging: Arc<Staging>,
    reservation: Reservation,
}

impl fmt::Debug for TierRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TierRange").field(&self.reservation).finish()
    }
}

impl TierRange {
    /// A block of its own on `staging`'s tier for `len` bytes. Blocking: it creates the
    /// block's file.
    ///
    /// # Errors
    /// The tier does not take it: no room, or the file could not be created.
    fn reserve(staging: &Arc<Staging>, len: u64) -> Result<Self, TierError> {
        let reservation = staging.tier.reserve_upload(len)?;
        Ok(Self {
            staging: Arc::clone(staging),
            reservation,
        })
    }

    /// Writes `bytes` at `offset` within the range. Blocking.
    ///
    /// # Errors
    /// Bytes outside the range, or the tier's write failed (which poisons the block).
    pub(crate) fn write(&self, offset: u64, bytes: &[u8]) -> Result<(), TierError> {
        self.staging.tier.write(&self.reservation, offset, bytes)
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// A range of `len` bytes on the write tier, in a block of its own; `None` without a
    /// tier or when the tier has no room for it. Not counted as a write the tier did not
    /// take: a segment upload's records held in memory instead are offered to the tier
    /// again at the commit, which counts.
    pub(crate) async fn reserve_range(&self, len: u64) -> Option<TierRange> {
        let staging = Arc::clone(self.writer.tier.get()?);
        let reserved = blocking(
            &self.writer.metrics,
            Box::new(move || TierRange::reserve(&staging, len)),
        );
        reserved.await.ok()
    }

    /// Makes `range` durable and its `blobs` (each at its offset in the range) present:
    /// the block synced, then one `PutStaged`, which seals it. Answers once they are
    /// indexed.
    ///
    /// # Errors
    /// The sync or the commit failed; nothing is stored.
    pub(crate) async fn stage_range(
        &self,
        range: TierRange,
        blobs: Vec<(Digest, u64)>,
    ) -> Result<(), CacheError> {
        let TierRange {
            staging,
            reservation,
        } = range;
        let tier = Arc::clone(&staging.tier);
        let synced = blocking(
            &self.writer.metrics,
            Box::new(move || tier.sync(reservation).map_err(|unsynced| unsynced.error)),
        )
        .await?;
        let at = synced.offset();
        let blobs = blobs
            .into_iter()
            .map(|(d, offset)| (d, at + offset))
            .collect();
        self.writer.put_staged(&staging, synced, blobs, true).await
    }

    /// A place in the write pool for a blob of `size` bytes ([`super::pool`]): the blob is
    /// appended to a shared segment by one of the pool's writers, not to a file of its
    /// own. `None` without a tier, when the pool's queue is full (counted as a write sent
    /// to the store), or when `size` is 0.
    pub(crate) fn tier_open(&self, size: u64) -> Option<Open> {
        Some(Open::start(self.pool_open(size)?))
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> Cache<M, O> {
    /// A stream for one blob of `size` bytes: onto the write tier, in a block of its
    /// own, when it has room for it; else (no tier, or no room) straight to the object
    /// store as the bytes are appended (a multipart upload on S3), the digest claimed
    /// at the commit and checked before the store completes the object. `None` only
    /// when `size` is 0 or the cache was made outside a runtime (it cannot stream at
    /// all). Blocking (it may create the block's file): call it off the async runtime.
    #[must_use]
    pub fn stream(self: &Arc<Self>, size: u64) -> Option<RecordStream<M, O>> {
        let target = match self.tier_open(size) {
            Some(open) => Target::Tier(open),
            None => Target::Store(ToStore::start(self, size)?),
        };
        let cache = Arc::clone(self);
        Some(RecordStream { cache, target })
    }
}

/// A record being written onto the write tier through the pool, or to the store
/// ([`Cache::stream`]). Dropped before [`StreamedRecord::commit`], nothing is stored: its
/// range on the tier is answered as failed, or its upload to the store aborted.
pub struct RecordStream<M, O> {
    cache: Arc<Cache<M, O>>,
    target: Target,
}

/// Where a [`RecordStream`] writes.
enum Target {
    Tier(Open),
    Store(ToStore),
}

/// A record whose bytes are all written, checked against its digest's size and its
/// CRC-32C ([`RecordStream::finish`]), not yet committed.
pub struct StreamedRecord<M, O> {
    cache: Arc<Cache<M, O>>,
    staged: Staged,
    digest: Digest,
}

/// Where a [`StreamedRecord`]'s bytes are.
enum Staged {
    Pool(PoolCommit),
    Store(Claim),
}

/// The part of a stream onto the tier that does not depend on the cache's types: its
/// place in the write pool and what was written to it.
pub(crate) struct Open {
    write: PoolWrite,
    /// Bytes written so far.
    written: u64,
    crc: Crc32c,
}

impl Open {
    fn start(write: PoolWrite) -> Self {
        Self {
            write,
            written: 0,
            crc: Crc32c::new(),
        }
    }

    /// Hands `chunk` to the pool. Blocking.
    ///
    /// # Errors
    /// [`Refused::NoRoom`] if the tier took no room for the blob (nothing is written, and
    /// the caller sends the blob to the store); more bytes than the stream was made for,
    /// or the pool stopped.
    pub(crate) fn append(&mut self, chunk: &[u8]) -> Result<(), Refused> {
        let after = self.written + chunk.len() as u64;
        if after > self.write.size() {
            // As the tier refused a write past its range.
            return Err(Refused::Failed(
                TierError::OutOfRange {
                    offset: self.written,
                    len: chunk.len() as u64,
                    reserved: self.write.size(),
                }
                .into(),
            ));
        }
        self.write.append(chunk)?;
        self.crc.update(chunk);
        self.written = after;
        Ok(())
    }

    /// Checks the bytes written against `digest` and `crc32c`, and ends them.
    fn finish(self, digest: Digest, crc32c: u32) -> Result<PoolCommit, CacheError> {
        checked(
            self.written,
            self.write.size(),
            digest,
            self.crc.finish(),
            crc32c,
        )?;
        Ok(self.write.end())
    }
}

/// Checks a stream of `written` bytes, made for `reserved`, whose CRC-32C is `crc`,
/// against `digest` and the caller's `crc32c`.
pub(super) fn checked(
    written: u64,
    reserved: u64,
    digest: Digest,
    crc: u32,
    crc32c: u32,
) -> Result<(), CacheError> {
    // Both parts are looked at, whichever is true: no short-circuit to cover.
    let short = (written != digest.size_bytes) | (written != reserved);
    if short {
        return Err(CacheError::Invalid(format!(
            "a stream of {written} bytes (made for {reserved}) ended as {digest}"
        )));
    }
    if crc != crc32c {
        return Err(CacheError::Invalid(format!(
            "{digest} was streamed with CRC-32C {crc32c:08x}, its bytes have {crc:08x}"
        )));
    }
    Ok(())
}

impl<M: MetaLog, O: ObjectStore> RecordStream<M, O> {
    /// Writes `chunk` after the bytes written so far. Blocking: call it off the async
    /// runtime.
    ///
    /// # Errors
    /// More bytes than the stream was made for, or the tier's write failed.
    pub fn append(&mut self, chunk: &[u8]) -> Result<(), CacheError> {
        match &mut self.target {
            Target::Tier(open) => match open.append(chunk) {
                Ok(()) => Ok(()),
                Err(Refused::Failed(e)) => Err(e),
                // The tier had no room for the blob when its writer took it: nothing was
                // written, so the blob goes to the store from its first chunk.
                Err(Refused::NoRoom) => {
                    let size = open.write.size();
                    // The pool runs on the cache's runtime, as does an upload to the store.
                    let none = CacheError::Invalid("the cache cannot stream this blob".to_owned());
                    let mut to_store = ToStore::start(&self.cache, size).ok_or(none)?;
                    to_store.append(chunk)?;
                    self.target = Target::Store(to_store);
                    Ok(())
                }
            },
            Target::Store(to_store) => to_store.append(chunk),
        }
    }

    /// Ends the stream as the blob `digest`, whose CRC-32C is `crc32c`.
    ///
    /// # Errors
    /// [`CacheError::Invalid`] when the bytes written are not as many as `digest` names
    /// and the stream was made for, or their CRC-32C is not `crc32c`: nothing is stored.
    pub fn finish(self, digest: Digest, crc32c: u32) -> Result<StreamedRecord<M, O>, CacheError> {
        let staged = match self.target {
            Target::Tier(open) => Staged::Pool(open.finish(digest, crc32c)?),
            Target::Store(to_store) => Staged::Store(to_store.finish(digest, crc32c)?),
        };
        Ok(StreamedRecord {
            cache: self.cache,
            staged,
            digest,
        })
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> Cache<M, O> {
    /// Stores the blob `digest`, whose bytes come in `bytes`, onto the write tier
    /// through `open` (made for its size by [`Cache::tier_open`]): each piece hashed and
    /// written to the stream's block off the runtime as it comes, holding one piece at
    /// a time; then, only if the bytes hash to `digest`, the block synced and committed
    /// as for [`StreamedRecord::commit`]. The flusher writes it to the store later. How
    /// a ByteStream `Write` over `MAX_UNSTAGED_BLOB_BYTES` is stored when the tier has
    /// room for it.
    ///
    /// # Errors
    /// [`CacheError::DigestMismatch`] if the bytes do not hash to `digest`; the body's
    /// own error; more bytes than the stream was made for, or the tier failed. Nothing
    /// is stored then: the stream is dropped, its range answered as failed.
    pub(crate) async fn stage_streamed(
        self: &Arc<Self>,
        digest: Digest,
        open: Open,
        mut bytes: BodyStream,
    ) -> Result<(), CacheError> {
        let mut writing = (
            RecordStream {
                cache: Arc::clone(self),
                target: Target::Tier(open),
            },
            RecordHasher::new(),
        );
        while let Some(piece) = bytes.next().await {
            let piece = piece?;
            let (mut stream, mut hasher) = writing;
            let write = move || {
                hasher.update(&piece);
                Ok(stream.append(&piece).map(|()| (stream, hasher)))
            };
            // Counted and timed as every tier call is.
            writing = blocking(&self.writer.metrics, Box::new(write)).await??;
        }
        let (stream, hasher) = writing;
        let (actual, crc32c) = hasher.finish();
        if actual != digest {
            return Err(CacheError::DigestMismatch {
                claimed: digest,
                actual,
            });
        }
        stream.finish(digest, crc32c)?.commit().await
    }
}

impl<M: MetaLog, O: ObjectStore> StreamedRecord<M, O> {
    /// The blob's digest.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Makes the record durable and present: on the tier, its block synced, then one
    /// `PutStaged` for it, which seals the block; to the store, its digest claimed, the
    /// bytes checked against it and the upload completed. Answers once it is indexed.
    ///
    /// # Errors
    /// The sync, the store or the commit failed, or (to the store) the bytes do not
    /// hash to the digest; the blob is not stored.
    pub async fn commit(self) -> Result<(), CacheError> {
        match self.staged {
            Staged::Pool(pooled) => self.cache.pool_commit(pooled, self.digest).await,
            Staged::Store(claim) => claim.commit(self.digest).await,
        }
    }
}
