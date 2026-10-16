//! Records written onto the write tier as their bytes arrive, never held whole in
//! memory: a [`TierRange`], a block of its own reserved for them, written chunk by
//! chunk, then synced and committed with one `PutStaged`, which seals the block.
//!
//! Two callers write through one:
//!
//! - **A segment upload on a write tier** (`crate::segments`): `Begin` reserves a range
//!   for each segment's records ([`Cache::reserve_range`]), each `PutRange` writes its
//!   bytes at their offset, and `Commit` stages the range with the records the footer
//!   declared ([`Cache::stage_range`]). What the upload holds in memory is the range
//!   being written, not the segment.
//! - **A record streamed by a daemon in the server's own process** (`kbf-all-in-one`'s
//!   plane), whose digest is known only at its end: [`Cache::stream`] reserves the range;
//!   [`RecordStream::append`] writes each chunk at the next offset as the caller reads
//!   it, and keeps the CRC-32C of what it wrote; [`RecordStream::finish`] checks that the
//!   bytes written are as many as the digest names and that their CRC-32C is the
//!   caller's; [`StreamedRecord::commit`] stages it.
//!
//! Either way the digests are trusted at the commit, as a segment upload's are with a
//! tier: the flusher hashes every record before its PUT and drops one that does not
//! match. Without a write tier, or when the tier has no room, there is no range: the
//! caller holds the bytes in memory as before.

use std::fmt;
use std::sync::Arc;

use kbf_objstore::ObjectStore;
use kbf_segments::Crc32c;
use kbf_tier::{Reservation, TierError};
use kbf_types::Digest;

use super::tier::{Staging, blocking};
use super::{Cache, CacheError};
use crate::MAX_BLOB_BYTES;
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

    /// The bytes the range holds.
    pub(crate) fn len(&self) -> u64 {
        self.reservation.len()
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

    /// A stream for one blob of `size` bytes onto the write tier, in a block of its own;
    /// `None` without a tier, when the tier has no room for it (counted as a write the
    /// tier did not take), or when `size` is 0 or over [`MAX_BLOB_BYTES`]. Blocking (it
    /// creates the block's file): call it off the async runtime.
    #[must_use]
    pub fn stream(self: &Arc<Self>, size: u64) -> Option<RecordStream<M, O>> {
        let staging = self.writer.tier.get()?;
        (1..=MAX_BLOB_BYTES).contains(&size).then_some(())?;
        let reserved = TierRange::reserve(staging, size);
        let range = reserved.inspect_err(|e| staging.counters.direct(e)).ok()?;
        Some(RecordStream {
            cache: Arc::clone(self),
            open: Open {
                range,
                written: 0,
                crc: Crc32c::new(),
            },
        })
    }
}

/// A record being written onto the write tier ([`Cache::stream`]). Dropped before
/// [`StreamedRecord::commit`], its range is answered as failed and nothing is stored.
pub struct RecordStream<M, O> {
    cache: Arc<Cache<M, O>>,
    open: Open,
}

/// A record whose bytes are all on the write tier, checked against its digest's size
/// and its CRC-32C ([`RecordStream::finish`]), not yet committed.
pub struct StreamedRecord<M, O> {
    cache: Arc<Cache<M, O>>,
    range: TierRange,
    digest: Digest,
}

/// The part of a stream that does not depend on the cache's types: its range on the
/// tier and what was written to it.
struct Open {
    range: TierRange,
    /// Bytes written so far: where the next chunk goes.
    written: u64,
    crc: Crc32c,
}

impl Open {
    fn append(&mut self, chunk: &[u8]) -> Result<(), CacheError> {
        self.range.write(self.written, chunk)?;
        self.crc.update(chunk);
        self.written += chunk.len() as u64;
        Ok(())
    }

    /// Checks the bytes written against `digest` and `crc32c`.
    fn check(&self, digest: Digest, crc32c: u32) -> Result<(), CacheError> {
        let reserved = self.range.len();
        // Both parts are looked at, whichever is true: no short-circuit to cover.
        let short = (self.written != digest.size_bytes) | (self.written != reserved);
        if short {
            return Err(CacheError::Invalid(format!(
                "a stream of {} bytes (made for {reserved}) ended as {digest}",
                self.written
            )));
        }
        let crc = self.crc.finish();
        if crc != crc32c {
            return Err(CacheError::Invalid(format!(
                "{digest} was streamed with CRC-32C {crc32c:08x}, its bytes have {crc:08x}"
            )));
        }
        Ok(())
    }
}

impl<M: MetaLog, O: ObjectStore> RecordStream<M, O> {
    /// Writes `chunk` after the bytes written so far. Blocking: call it off the async
    /// runtime.
    ///
    /// # Errors
    /// More bytes than the stream was made for, or the tier's write failed.
    pub fn append(&mut self, chunk: &[u8]) -> Result<(), CacheError> {
        self.open.append(chunk)
    }

    /// Ends the stream as the blob `digest`, whose CRC-32C is `crc32c`.
    ///
    /// # Errors
    /// [`CacheError::Invalid`] when the bytes written are not as many as `digest` names
    /// and the stream was made for, or their CRC-32C is not `crc32c`: nothing is stored.
    pub fn finish(self, digest: Digest, crc32c: u32) -> Result<StreamedRecord<M, O>, CacheError> {
        self.open.check(digest, crc32c)?;
        Ok(StreamedRecord {
            cache: self.cache,
            range: self.open.range,
            digest,
        })
    }
}

impl<M: MetaLog, O: ObjectStore> StreamedRecord<M, O> {
    /// The blob's digest.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Makes the record durable and present: its block synced, then one `PutStaged`
    /// for it, which seals the block. Answers once it is indexed.
    ///
    /// # Errors
    /// The sync or the commit failed; the blob is not stored.
    pub async fn commit(self) -> Result<(), CacheError> {
        let blobs = vec![(self.digest, 0)];
        self.cache.stage_range(self.range, blobs).await
    }
}
