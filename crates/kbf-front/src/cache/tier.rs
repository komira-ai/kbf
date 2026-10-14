//! The local write tier as the cache uses it (`docs/design/write-path.md`, section 6):
//! packed segments staged in block files instead of PUT, staged and flushed blobs read
//! from the local disk, and the start that ties a tier directory to the committed
//! state. The flusher is in `flush.rs`; the disk layer is `kbf-tier`.
//!
//! **The write seam.** [`Cache::place_segment`] is where a packed segment goes: staged
//! when a tier is attached and takes it, else PUT. `store_blobs` calls it for every
//! segment it packs; the write grouping of rollout stage 1 calls it for every segment
//! a group seals, once both are on one branch.

#![allow(
    dead_code,
    unreachable_code,
    unused_variables,
    reason = "red-first stand-in: the next commit implements the write tier's flusher, reads and start"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kbf_meta::{
    Applied, Command, Location, ObjectId, ServerId, StagedPut, TierId, UnreachableReason,
};
use kbf_objstore::ObjectStore;
use kbf_segments::{Footer, IndexEntry, SegmentWriter};
use kbf_store::Fs;
use kbf_tier::{
    NextSeq, Recovery, StagedSnapshot, Tier, TierError, TierOptions, TierStart, WriteTier,
};
use kbf_types::Digest;

use super::{Cache, CacheError, unexpected};
use crate::meta_log::MetaLog;
use crate::metrics::TierCounters;

/// The tier a cache stages its writes on, what it counted, and the flusher's retries.
pub(super) struct Staging {
    pub(super) tier: Arc<dyn WriteTier>,
    pub(super) id: TierId,
    pub(super) counters: TierCounters,
    retry: Mutex<BTreeMap<ObjectId, Retry>>,
}

/// A flush that failed: how many times in a row, and when the next may start.
#[derive(Clone, Copy, Debug)]
struct Retry {
    failures: u32,
    next: Instant,
}

impl fmt::Debug for Staging {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Staging")
            .field("id", &self.id)
            .field("usage", &self.tier.usage())
            .finish_non_exhaustive()
    }
}

impl Staging {
    fn new(tier: Arc<dyn WriteTier>, id: TierId) -> Self {
        Self {
            tier,
            id,
            counters: TierCounters::default(),
            retry: Mutex::new(BTreeMap::new()),
        }
    }

    fn retries(&self) -> MutexGuard<'_, BTreeMap<ObjectId, Retry>> {
        self.retry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The objects whose next flush attempt is still in the future at `now`.
    pub(super) fn waiting(&self, now: Instant) -> BTreeSet<ObjectId> {
        let retries = self.retries();
        let waiting = retries.iter().filter(|(_, r)| r.next > now);
        waiting.map(|(object, _)| *object).collect()
    }

    /// Records a failed flush of `object` at `now`: the next attempt waits
    /// [`flush_backoff`] of the failures in a row.
    pub(super) fn failed(&self, object: ObjectId, now: Instant) -> Duration {
        let mut retries = self.retries();
        let failures = retries.get(&object).map_or(0, |r| r.failures) + 1;
        let wait = super::flush::flush_backoff(failures);
        let next = now + wait;
        retries.insert(object, Retry { failures, next });
        wait
    }

    /// Forgets `object`'s failures: it left the flusher's queue.
    pub(super) fn done(&self, object: ObjectId) {
        self.retries().remove(&object);
    }
}

/// Where [`Cache::stage`] left a segment.
enum Staged {
    /// Synced and committed: the blobs are indexed and staged.
    Committed,
    /// The tier did not take it (full, or a disk error before the commit): PUT it.
    Direct,
}

/// What a local read found.
enum Local {
    Read(Bytes),
    /// The bytes are there but fail their digest.
    Damaged,
    /// No local copy (never staged here, flushed and evicted, or unreadable).
    Absent,
}

/// What [`Cache::attach_tier`] found and did at this start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierStartup {
    /// The tier this server stages on now.
    pub tier: TierId,
    /// Whether this start registered it (`AddTier`): a directory used for the first
    /// time, or the one after `--write-tier-abandon`.
    pub registered: bool,
    /// The objects `--write-tier-abandon` marked unreachable as missing.
    pub abandoned: Vec<ObjectId>,
    /// What the disk layer's open found and did. Its `lost` objects are marked
    /// unreachable as missing by the time this is returned.
    pub recovery: Recovery,
}

/// Why [`Cache::attach_tier`] refused the start.
#[derive(Debug, thiserror::Error)]
pub enum TierStartError {
    /// The metadata log could not answer.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// The directory is not this server's tier, or the disk failed: a wrong mount or
    /// flag must not destroy the index.
    #[error(transparent)]
    Tier(#[from] TierError),
    /// `--write-tier-abandon` names a tier that is not this server's.
    #[error("--write-tier-abandon names {named}, but this server's tier is {}", current.map_or_else(|| "none".to_owned(), |t| t.to_string()))]
    AbandonOther {
        /// The tier the flag names.
        named: TierId,
        /// The server's registered tier.
        current: Option<TierId>,
    },
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Opens the tier directory `fs` as `server`'s write tier for this start
    /// (`docs/design/write-path.md`, section 6.4), and stages every later write on it.
    ///
    /// 1. With `abandon`, which must name `server`'s registered tier, every object
    ///    staged there is marked unreachable as missing (its blobs answer
    ///    `Unavailable` until re-uploaded), and a new tier is registered.
    /// 2. The tier is the registered one, or, for a server without one, a new id: this
    ///    start's writer epoch, which no tier was ever registered under.
    /// 3. The disk layer opens the directory against the committed staged table
    ///    ([`Tier::open`]): the `TIER` check (a wrong or missing `TIER` while objects
    ///    are staged refuses the start), each staged block cut to its committed length,
    ///    and the orphan sweep. A new tier's `TIER` is synced before its `AddTier`
    ///    commits.
    /// 4. Each staged object whose block is missing or short is marked unreachable as
    ///    missing, and logged as an error: only that object, not the tier.
    ///
    /// Unsealed recovered blocks are sealed, and sealed ones flushed, by
    /// [`Cache::flush_tier`].
    ///
    /// # Errors
    ///
    /// [`TierStartError`]; the cache is dropped and the server must not start.
    pub async fn attach_tier<F>(
        mut self,
        fs: F,
        options: TierOptions,
        server: ServerId,
        abandon: Option<TierId>,
    ) -> Result<(Self, TierStartup), TierStartError>
    where
        F: Fs + Send + Sync + fmt::Debug + 'static,
        F::File: Send + Sync + fmt::Debug + 'static,
    {
        let mut current = self
            .query(move |s| s.tier(server))
            .await
            .map_err(CacheError::from)?;
        let mut abandoned = Vec::new();
        if let Some(named) = abandon {
            if current != Some(named) {
                return Err(TierStartError::AbandonOther { named, current });
            }
            abandoned = self
                .query(move |s| {
                    let on_it = s.staged_objects().filter(|(_, o)| o.tier() == named);
                    on_it.map(|(object, _)| object).collect::<Vec<_>>()
                })
                .await
                .map_err(CacheError::from)?;
            for object in abandoned.iter().filter(|_| false) {
                self.mark_unreachable(*object, UnreachableReason::Missing)
                    .await?;
            }
            tracing::warn!(%named, objects = abandoned.len(), "write tier abandoned");
            current = None;
        }
        let tier = current.unwrap_or_else(|| TierId::new(self.epoch.get()));
        let snapshot = self
            .query(StagedSnapshot::of)
            .await
            .map_err(CacheError::from)?;
        let start = TierStart {
            tier,
            epoch: self.epoch,
            options,
        };
        let (opened, recovery) = Tier::open(fs, start, &snapshot, self.seq_source())?;
        if current.is_none() {
            let added = self.commit(Command::AddTier { server, tier }).await;
            match added.map_err(CacheError::from)? {
                Applied::Tier(Ok(())) => {}
                other => return Err(unexpected(&other).into()),
            }
        }
        for object in recovery.lost.iter().filter(|_| false) {
            tracing::error!(%object, %tier, "a staged block is missing or short; its blobs are unavailable until uploaded again");
            self.mark_unreachable(*object, UnreachableReason::Missing)
                .await?;
        }
        self.tier = Some(Staging::new(Arc::new(opened), tier));
        let startup = TierStartup {
            tier,
            registered: current.is_none(),
            abandoned,
            recovery,
        };
        Ok((self, startup))
    }

    /// The tier this cache stages on, if one is attached.
    pub fn tier_id(&self) -> Option<TierId> {
        self.tier.as_ref().map(|s| s.id)
    }

    /// The attached tier's disk layer, if one is attached: its usage and exposure.
    pub fn write_tier(&self) -> Option<&dyn WriteTier> {
        self.tier.as_ref().map(|s| &*s.tier)
    }

    /// Every metric family: [`CacheMetrics::render`](crate::metrics::CacheMetrics::render),
    /// then, with a tier attached, the tier's (its counters, its budget and usage, and
    /// the unflushed bytes and oldest unflushed block's age, measured now).
    #[must_use]
    pub fn render_metrics(&self) -> String {
        return self.metrics.render();
        let mut out = self.metrics.render();
        if let Some(staging) = &self.tier {
            let usage = staging.tier.usage();
            let unflushed = staging.tier.unflushed(Instant::now());
            staging.counters.render(&mut out, usage, unflushed);
        }
        out
    }

    /// Numbers the tier's objects from this cache's sequence, so a staged object and a
    /// PUT one never share an id.
    fn seq_source(&self) -> NextSeq {
        let seq = Arc::clone(&self.next_seq);
        Box::new(move || seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    /// Writes one packed segment where it goes: staged on the tier (in the open
    /// grouped block, or, for `own_block`, a block of its own sealed by its commit)
    /// when a tier is attached and takes it, else PUT to the store. Returns the
    /// blobs' locations for the caller's `PutBlobs`, or `None` when the tier's
    /// `PutStaged` already indexed them.
    ///
    /// A tier that is full, or whose disk fails before the commit, sends the segment
    /// to the store: backpressure, never loss. With the store down too, that fails and
    /// the caller is answered UNAVAILABLE.
    ///
    /// # Errors
    ///
    /// The store or the metadata log failed; nothing was reported present.
    pub(super) async fn place_segment(
        &self,
        writer: SegmentWriter,
        packed: &[Digest],
        own_block: bool,
    ) -> Result<Option<Vec<(Digest, Location)>>, CacheError> {
        let body = Bytes::from(writer.finish());
        return self.put_body(body, packed).await.map(Some);
        if let Some(staging) = &self.tier
            && let Staged::Committed = self.stage(staging, body.clone(), own_block).await?
        {
            return Ok(None);
        }
        self.put_body(body, packed).await.map(Some)
    }

    /// Stages one finished segment: its records written to a reserved range, synced,
    /// then `PutStaged` for every record. The footer is not written; the flusher builds
    /// it.
    async fn stage(
        &self,
        staging: &Staging,
        body: Bytes,
        own_block: bool,
    ) -> Result<Staged, CacheError> {
        let footer = Footer::parse(&body, body.len() as u64)?;
        let records = body.slice(..usize::try_from(footer.records_len()).unwrap_or(body.len()));
        let len = records.len() as u64;
        let tier = Arc::clone(&staging.tier);
        let synced = blocking(Box::new(move || {
            let reservation = if own_block {
                tier.reserve_upload(len)?
            } else {
                tier.reserve_group(len, Instant::now())?
            };
            tier.write(&reservation, 0, &records)?;
            tier.sync(reservation).map_err(|unsynced| unsynced.error)
        }))
        .await;
        let synced = match synced {
            Ok(synced) => synced,
            Err(e) => {
                staging.counters.direct(&e);
                tracing::debug!(error = %e, len, "the write tier did not take a segment; writing it to the store");
                return Ok(Staged::Direct);
            }
        };
        let at = |e: &IndexEntry| (e.digest, synced.offset() + e.offset);
        let put = StagedPut {
            object: synced.object(),
            end: synced.end(),
            seal: own_block,
            blobs: footer.entries().iter().map(at).collect(),
        };
        let command = Command::PutStaged {
            tier: staging.id,
            objects: vec![put],
        };
        // A failed commit drops `synced` unanswered: the range is answered as failed.
        match self.commit(command).await? {
            Applied::Staged(Ok(_)) => {
                staging.tier.committed(synced);
                staging.counters.staged.add(1);
                Ok(Staged::Committed)
            }
            other => Err(unexpected(&other)),
        }
    }

    /// Reads `digest` at `location` as the index answered it: from this server's tier
    /// when the object is staged on it, or has a flushed copy here, else from the
    /// store. An object staged when the query ran is never marked unreachable by a
    /// read that does not find it in the store (it is not there yet); a staged record
    /// that fails its digest on the local disk marks it corrupt (the local copy is the
    /// only one). A blob staged on another server's tier is not read here.
    pub(super) async fn fetch_answer(
        &self,
        digest: &Digest,
        location: Location,
        staged_on: Option<TierId>,
    ) -> Result<Bytes, CacheError> {
        return self.fetch_from_store(digest, location, true).await;
        if let Some(staging) = &self.tier
            && staged_on.is_none_or(|t| t == staging.id)
        {
            match self.read_local(staging, digest, location).await {
                Local::Read(bytes) => return Ok(bytes),
                Local::Damaged if staged_on.is_some() => {
                    self.mark_unreachable(location.object, UnreachableReason::Corrupt)
                        .await?;
                    return Err(CacheError::Unreachable(*digest));
                }
                Local::Damaged | Local::Absent => {}
            }
        }
        self.fetch_from_store(digest, location, staged_on.is_none())
            .await
    }

    /// `digest`'s record from the tier's block or flushed copy, checked.
    async fn read_local(&self, staging: &Staging, digest: &Digest, at: Location) -> Local {
        let len = usize::try_from(digest.size_bytes).unwrap_or(usize::MAX);
        let tier = Arc::clone(&staging.tier);
        let read = blocking(Box::new(move || tier.read(at.object, at.offset, len))).await;
        match read {
            Ok(bytes) if kbf_segments::sha256(&bytes) == *digest => {
                staging.counters.local_reads.add(1);
                Local::Read(Bytes::from(bytes))
            }
            Ok(_) => {
                tracing::error!(%digest, object = %at.object, "a record on the write tier fails its digest");
                Local::Damaged
            }
            Err(TierError::UnknownBlock(_)) => Local::Absent,
            Err(e) => {
                tracing::warn!(%digest, error = %e, "write tier read failed; reading the store");
                Local::Absent
            }
        }
    }
}

/// Runs a tier call on the blocking pool: the tier's file I/O (creates, writes,
/// `fdatasync`, reads of up to a segment) must not hold a runtime thread. A call that
/// panicked is a failed call.
pub(super) async fn blocking<T: Send + 'static>(
    call: Box<dyn FnOnce() -> Result<T, TierError> + Send>,
) -> Result<T, TierError> {
    let joined = tokio::task::spawn_blocking(call).await;
    joined.unwrap_or_else(|e| {
        Err(TierError::Io {
            op: "run",
            name: "a write tier call".to_owned(),
            source: std::io::Error::other(e.to_string()),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a tier call that panicked on the blocking pool taken for a success, or
    /// tearing down the caller, instead of failing that one call.
    #[tokio::test]
    async fn a_tier_call_that_panics_is_a_failed_call() {
        let ok = blocking::<u8>(Box::new(|| Ok(7))).await;
        assert!(matches!(ok, Ok(7)), "{ok:?}");
        let panicked = blocking::<u8>(Box::new(|| panic!("a tier call panicked"))).await;
        assert!(
            matches!(panicked, Err(TierError::Io { op: "run", .. })),
            "{panicked:?}"
        );
    }
}
