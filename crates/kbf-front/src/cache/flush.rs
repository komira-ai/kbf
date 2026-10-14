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
//! - **A record that fails its digest** on re-read: not uploaded; the object is marked
//!   unreachable as corrupt and its block discarded.
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
    /// Objects with a record that failed its digest: marked unreachable as corrupt.
    pub corrupt: Vec<ObjectId>,
    /// Objects whose flush failed this round, and how long until their next attempt.
    pub retrying: Vec<(ObjectId, Duration)>,
}

/// How one flush ended, short of an error to retry.
enum Flushed {
    Done,
    Conflict,
    Corrupt,
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
        let Some(staging) = &self.tier else {
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
                Ok(Flushed::Corrupt) => {
                    staging.done(object);
                    round.corrupt.push(object);
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
        let Some(entries) = entries(&records, blobs) else {
            tracing::error!(%object, "a staged record fails its digest; the object is not flushed and is marked corrupt");
            self.mark_unreachable(object, UnreachableReason::Corrupt)
                .await?;
            let _ = staging.tier.discard(object);
            staging.counters.corrupt.add(1);
            return Ok(Flushed::Corrupt);
        };
        let footer = encode_footer(len, &entries);
        let total = len + footer.len() as u64;
        let ours = Footer::parse(&footer, total)?;
        let mut body = records;
        body.extend_from_slice(&footer);
        let key = self.object_key(object)?;
        let existed = match self.objects.put_new(&key, Bytes::from(body), None).await {
            Ok(()) => false,
            Err(ObjectStoreError::AlreadyExists(_)) => true,
            Err(e) => return Err(e.into()),
        };
        let stored = match self.read_footer(&key, total).await {
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

    /// Commits a tier command whose refusal would mean the flusher and the committed
    /// state disagree: anything but its success is an internal error.
    async fn commit_tier(&self, command: Command) -> Result<(), CacheError> {
        match self.commit(command).await? {
            Applied::Tier(Ok(())) => Ok(()),
            other => Err(unexpected(&other)),
        }
    }
}

/// The footer's index entries for `blobs` (sorted by digest) over `records`, or `None`
/// if a record is not all there or fails its digest.
fn entries(records: &[u8], blobs: &[(Digest, u64)]) -> Option<Vec<IndexEntry>> {
    let mut out = Vec::with_capacity(blobs.len());
    for (digest, offset) in blobs {
        let from = usize::try_from(*offset).ok()?;
        let to = from.checked_add(usize::try_from(digest.size_bytes).ok()?)?;
        let record = records.get(from..to)?;
        if kbf_segments::sha256(record) != *digest {
            return None;
        }
        out.push(IndexEntry {
            digest: *digest,
            offset: *offset,
            crc32c: kbf_segments::crc32c(record),
        });
    }
    Some(out)
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
