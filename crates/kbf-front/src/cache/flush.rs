//! The write tier's flusher (`docs/design/write-path.md`, sections 6.1.4 and 6.4):
//! seals, then each sealed staged object read back from its block, checked, given its
//! footer, PUT under its usual key, its footer read back, and `ObjectFlushed`
//! committed, off the ack path.
//!
//! - **Same key, same bytes.** An object's key is fixed by its id, and its footer is
//!   built from the committed blob list (sorted by digest) and the bytes on disk, so
//!   every attempt, before or after a crash, PUTs identical content. A flush is never
//!   moved to another object.
//! - **`AlreadyExists`** (a store with conditional writes, after an earlier attempt
//!   whose answer was lost): the footer is read back; if it is ours the flush is done,
//!   else `FlushConflict` commits, the object stays staged and is read locally, and the
//!   flusher stops retrying it.
//! - **Every record is hashed** before the PUT, on the cache's hashing pool (off the
//!   runtime workers): the daemon's digests were trusted when the upload was
//!   acknowledged, so this is where they are checked. A record that fails its digest
//!   is never uploaded: it is counted (`kbf_tier_flush_digest_mismatch_total`), logged
//!   as an error with its digest and object, and dropped. The object is marked
//!   unreachable as corrupt and its block discarded, and the records that did hash to
//!   their digests are staged again as an object of their own (one more block, one
//!   more flush), so the rest of the segment still reaches the store and stays
//!   readable; only the bad digest is left unavailable, to be uploaded again.
//! - **Anything else** (the store or the log unavailable, a read-back that disagrees
//!   after our own PUT, a disk read error) is retried with backoff,
//!   [`FLUSH_BACKOFF_FIRST`] doubling to [`FLUSH_BACKOFF_MAX`].
//! - **Eviction is on committed state:** the block becomes a flushed copy
//!   ([`kbf_tier::WriteTier::flushed`]) only after `ObjectFlushed` applied.

use std::time::{Duration, Instant};

use bytes::Bytes;
use kbf_meta::{Applied, Command, ObjectId, StagedObject, StagedState, UnreachableReason};
use kbf_objstore::{ObjectStore, ObjectStoreError};
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry};
use kbf_types::Digest;

use super::tier::{Staging, blocking};
use super::upload::BuiltSegment;
use super::verify::{Verified, verify_records};
use super::{Cache, CacheError, unexpected};
use crate::meta_log::MetaLog;

/// The wait after a flush's first failure.
pub const FLUSH_BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// The longest wait between attempts at one flush.
pub const FLUSH_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The most objects one [`Cache::flush_tier`] round flushes, oldest first; the rest
/// wait for the next round.
pub const FLUSHES_PER_ROUND: usize = 4;

/// The wait before the next attempt after `failures` failures in a row.
pub(super) fn flush_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FLUSH_BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(FLUSH_BACKOFF_MAX)
}

/// What one [`Cache::flush_tier`] round did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FlushRound {
    /// Objects whose `SealStaged` committed.
    pub sealed: Vec<ObjectId>,
    /// Objects flushed: in the store, `ObjectFlushed` committed.
    pub flushed: Vec<ObjectId>,
    /// Objects whose key holds another object: `FlushConflict` committed.
    pub conflicts: Vec<ObjectId>,
    /// Objects with a record that failed its digest: marked unreachable as corrupt, the
    /// records that were sound staged again as an object of their own.
    pub corrupt: Vec<ObjectId>,
    /// The digests of the records that failed theirs, in the order they were found.
    pub dropped: Vec<Digest>,
    /// Objects whose flush failed this round, and how long until their next attempt.
    pub retrying: Vec<(ObjectId, Duration)>,
}

/// How one flush ended, short of an error to retry.
enum Flushed {
    Done,
    Conflict,
    /// Records failed their digests; these are their digests.
    Corrupt(Vec<Digest>),
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// One round of the write tier's background work at `now`; without a tier, none.
    ///
    /// 1. Every block whose seal is due and whose ranges are all answered gets its
    ///    `SealStaged` ([`kbf_tier::WriteTier::poll_seals`]).
    /// 2. Up to [`FLUSHES_PER_ROUND`] sealed objects staged on this tier, in id order
    ///    (oldest first), whose backoff has passed, are flushed. The queue is the
    ///    committed state's, so a restart resumes every flush it had not finished.
    ///
    /// # Errors
    ///
    /// The metadata log could not answer a seal or the queue's query. A failed flush
    /// is not an error: it is in [`FlushRound::retrying`].
    pub async fn flush_tier(&self, now: Instant) -> Result<FlushRound, CacheError> {
        let Some(staging) = self.writer.tier.get() else {
            return Ok(FlushRound::default());
        };
        let mut round = FlushRound::default();
        for object in staging.tier.poll_seals(now) {
            self.commit_tier(Command::SealStaged { object }).await?;
            // Named by `poll_seals` just now, so the tier takes it.
            let _ = staging.tier.sealed(object);
            round.sealed.push(object);
        }
        let (tier, waiting) = (staging.id, staging.waiting(now));
        let due = self
            .writer
            .query(move |s| {
                let ours = |o: &StagedObject| (o.tier(), o.state()) == (tier, StagedState::Sealed);
                let sealed = s.staged_objects().filter(|(_, o)| ours(o));
                let sealed = sealed.filter(|(object, _)| !waiting.contains(object));
                let due = sealed.take(FLUSHES_PER_ROUND).map(|(object, o)| {
                    let blobs: Vec<(Digest, u64)> = o.blobs().map(|(d, at)| (*d, at)).collect();
                    (object, o.committed_len(), blobs)
                });
                due.collect::<Vec<_>>()
            })
            .await?;
        for (object, len, blobs) in due {
            match self.flush_one(staging, object, len, &blobs).await {
                Ok(Flushed::Done) => {
                    staging.done(object);
                    round.flushed.push(object);
                }
                Ok(Flushed::Conflict) => {
                    staging.done(object);
                    round.conflicts.push(object);
                }
                Ok(Flushed::Corrupt(digests)) => {
                    staging.done(object);
                    round.corrupt.push(object);
                    round.dropped.extend(digests);
                }
                Err(e) => {
                    staging.counters.flush_failures.add(1);
                    let wait = staging.failed(object, now);
                    tracing::warn!(%object, error = %e, ?wait, "flush failed; retrying after the wait");
                    round.retrying.push((object, wait));
                }
            }
        }
        Ok(round)
    }

    /// Flushes `object`, staged with committed length `len` and `blobs` (sorted by
    /// digest, as the committed list is).
    async fn flush_one(
        &self,
        staging: &Staging,
        object: ObjectId,
        len: u64,
        blobs: &[(Digest, u64)],
    ) -> Result<Flushed, CacheError> {
        let tier = std::sync::Arc::clone(&staging.tier);
        let n = usize::try_from(len).unwrap_or(usize::MAX);
        let records = blocking(Box::new(move || tier.read(object, 0, n))).await?;
        let listed = blobs.to_vec();
        let (records, found) = self
            .hashing()
            .compute(move || {
                let found = verify_records(&records, &listed);
                (records, found)
            })
            .await?;
        if !found.bad.is_empty() {
            return self.drop_mismatched(staging, object, &records, found).await;
        }
        let entries = found.good;
        let footer = encode_footer(len, &entries);
        let total = len + footer.len() as u64;
        let ours = Footer::parse(&footer, total)?;
        let mut body = records;
        body.extend_from_slice(&footer);
        let key = self.object_key(object)?;
        let existed = match self
            .writer
            .objects
            .put_new(&key, Bytes::from(body), None)
            .await
        {
            Ok(()) => false,
            Err(ObjectStoreError::AlreadyExists(_)) => true,
            Err(e) => return Err(e.into()),
        };
        let stored = match self.writer.read_footer(&key, total).await {
            Ok(footer) => Some(footer),
            // The key holds an object this footer does not describe (another length,
            // or no footer where ours ends).
            Err(
                CacheError::Segment(_)
                | CacheError::Internal(_)
                | CacheError::Store(ObjectStoreError::InvalidRange { .. }),
            ) => None,
            Err(e) => return Err(e),
        };
        if stored.as_ref() == Some(&ours) {
            self.commit_tier(Command::ObjectFlushed { object }).await?;
            if let Err(e) = staging.tier.flushed(object) {
                tracing::warn!(%object, error = %e, "flushed object's block not kept as a local copy");
            }
            staging.counters.flushes.add(1);
            return Ok(Flushed::Done);
        }
        if !existed {
            // Our own PUT read back as something else: a store fault. The next attempt
            // PUTs the same bytes again (or meets them, or a conflict, as AlreadyExists).
            return Err(CacheError::Internal(format!(
                "object {key} read back with another footer after its PUT"
            )));
        }
        tracing::error!(%object, %key, "the store holds another object under a staged object's key; it stays staged, read locally, until an operator acts");
        self.commit_tier(Command::FlushConflict { object }).await?;
        staging.counters.conflicts.add(1);
        Ok(Flushed::Conflict)
    }

    /// Handles a staged object with records that fail their digests: counts and logs
    /// each (never uploaded), marks the object unreachable as corrupt (its blobs are
    /// unavailable, and a re-upload heals them), frees its block, and stages the
    /// records that hashed to their digests again as an object of their own. A failure
    /// of that last step is logged, not retried: the object has left the flusher's
    /// queue, and those blobs are unavailable until uploaded again.
    async fn drop_mismatched(
        &self,
        staging: &Staging,
        object: ObjectId,
        records: &[u8],
        found: Verified,
    ) -> Result<Flushed, CacheError> {
        for bad in &found.bad {
            tracing::error!(%object, digest = %bad.claimed, found = %bad.actual, "a staged record fails its digest; it is dropped and never flushed");
        }
        staging.counters.digest_mismatch.add(found.bad.len() as u64);
        staging.counters.corrupt.add(1);
        self.mark_unreachable(object, UnreachableReason::Corrupt)
            .await?;
        let _ = staging.tier.discard(object);
        if let Err(e) = self.restage(records, &found.good).await {
            tracing::error!(%object, error = %e, sound = found.good.len(), "the sound records of a corrupt staged object could not be staged again; they are unavailable until uploaded again");
        }
        let dropped = found.bad.iter().map(|m| m.claimed).collect();
        Ok(Flushed::Corrupt(dropped))
    }

    /// Stages `good` (digest-sorted records found at their offsets in `records`) as a
    /// segment of their own, as an upload does: on the tier, else PUT.
    async fn restage(&self, records: &[u8], good: &[IndexEntry]) -> Result<(), CacheError> {
        if good.is_empty() {
            return Ok(());
        }
        let mut body = Vec::new();
        let mut entries = Vec::with_capacity(good.len());
        for entry in good {
            // Hashed in range by `verify_records`.
            let from = entry.offset as usize;
            let to = from + entry.digest.size_bytes as usize;
            entries.push(IndexEntry {
                offset: body.len() as u64,
                ..*entry
            });
            body.extend_from_slice(&records[from..to]);
        }
        let footer = encode_footer(body.len() as u64, &entries);
        body.extend_from_slice(&footer);
        let sound = BuiltSegment {
            body: Bytes::from(body),
            entries,
            verified: true,
        };
        self.store_upload(vec![sound]).await.map(drop)
    }

    /// Commits a tier command whose refusal would mean the flusher and the committed
    /// state disagree: anything but its success is an internal error.
    async fn commit_tier(&self, command: Command) -> Result<(), CacheError> {
        match self.writer.commit(command).await? {
            Applied::Tier(Ok(())) => Ok(()),
            other => Err(unexpected(&other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a backoff that does not grow (a store outage met by a retry every
    /// round), one that grows without bound (a flush that waits hours after the store
    /// came back), and an overflow at many failures.
    #[test]
    fn backoff_doubles_from_the_first_wait_to_the_cap() {
        assert_eq!(flush_backoff(1), FLUSH_BACKOFF_FIRST);
        assert_eq!(flush_backoff(2), FLUSH_BACKOFF_FIRST * 2);
        assert_eq!(flush_backoff(3), FLUSH_BACKOFF_FIRST * 4);
        assert_eq!(flush_backoff(7), FLUSH_BACKOFF_MAX);
        assert_eq!(flush_backoff(u32::MAX), FLUSH_BACKOFF_MAX);
    }
}
