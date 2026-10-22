//! The blobs a node holds in RAM and has acknowledged but not yet written to its disk
//! (`docs/design/storage-ram-ack.md`), and the order every reader consults them in.
//!
//! **The producer** registers a blob when a write is acknowledged from RAM
//! ([`Cache::hold_write`], from `StreamedPut::commit` with `Held`, on a node with
//! `--ram-ack-bytes` over 0) and the drain removes it once the blob is in the index. With
//! the flag at 0 the set is empty and every reader answers from the index alone, as before.
//! A test can also put blobs in through [`Cache::hold_for_test`] (the `testkit` feature).
//!
//! **Held first, then the index.** A reader looks at this set, then at the index, as two
//! steps in that order (the index is queried inside one `MetaState` closure, so they cannot
//! be one snapshot). A blob leaves the set only after the index has it, so a reader that
//! misses here finds it there; the other order could find it in neither (index before the
//! blob lands, set after it left). A blob that is in both is served from here.
//!
//! **What counts as present.** A held blob is present: the node can serve exactly its
//! bytes now (`FindMissing`, `QueryWriteStatus`, every read, a write's dedup). It is not
//! *durable*: a restart loses it. A caller that writes something durable naming a blob
//! asks [`Cache::durable`], which answers from the index alone. A caller that stores a
//! blob ([`Cache::store_blobs`]) promises it is durable on return, so its own dedup
//! ([`Cache::held`]) looks at the index only and writes a blob the set holds.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use bytes::Bytes;
use kbf_meta::Closure;
use kbf_types::Digest;

use super::ram::Permit;
use super::{Cache, CacheError};
use crate::meta_log::MetaLog;
use kbf_objstore::ObjectStore;

/// A test's callback between a reader's two steps ([`Cache::with_between_hook`]).
#[cfg(feature = "testkit")]
pub type BetweenHook =
    std::sync::Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// A [`BetweenHook`] a `Debug` struct can hold.
#[cfg(feature = "testkit")]
pub(super) struct BetweenCell(pub(super) BetweenHook);

#[cfg(feature = "testkit")]
impl std::fmt::Debug for BetweenCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BetweenHook")
    }
}

/// How long [`Cache::durable_waiting`] and the action-cache barrier wait for held blobs to
/// be written unless [`Cache::with_durable_wait`] says otherwise (`--durable-wait`).
pub const DEFAULT_DURABLE_WAIT: Duration = Duration::from_secs(10);

/// Where a held blob is in its way to the disk (`docs/design/storage-ram-ack.md`, "Drain
/// and shutdown").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainState {
    /// Waiting for a drain pass.
    Queued,
    /// A drain pass has it, after `failures` failed passes in a row.
    Writing {
        /// Failed passes before this one.
        failures: u32,
    },
    /// The last pass failed `n` times in a row; the next one waits until `next`.
    Failing {
        /// Failed passes in a row.
        n: u32,
        /// The earliest the next pass takes it.
        next: Instant,
    },
}

/// One held blob: its bytes, the budget it holds, and where its drain is.
#[derive(Debug)]
pub(super) struct Entry {
    bytes: Bytes,
    /// Returned to the budget when the entry is dropped, which is when the blob leaves
    /// the set (`None` for a blob held without a budget, by a test).
    _permit: Option<Permit>,
    state: DrainState,
    since: Instant,
    /// The map generation the node held when the blob was acknowledged (0: not known), by
    /// which the flush report dates it as it dates a staged object.
    born: u64,
}

/// What the set holds in all: the bytes and blobs not yet in the index, and the age of
/// the oldest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unwritten {
    /// Bytes of the held blobs.
    pub bytes: u64,
    /// How many blobs are held.
    pub blobs: u64,
    /// How long the oldest has been held; zero with none.
    pub oldest: Duration,
}

/// The set: digest to the verified bytes and their drain.
#[derive(Debug, Default)]
pub(super) struct Held {
    blobs: Mutex<BTreeMap<Digest, Entry>>,
    /// Woken when an entry has left, for a caller waiting for digests to be written.
    left: tokio::sync::Notify,
}

impl Held {
    fn blobs(&self) -> MutexGuard<'_, BTreeMap<Digest, Entry>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `bytes` under `digest` unless the digest is held already; whether it did. A
    /// second writer of a digest is a dedup answer: it takes no entry and leaves the first
    /// writer's, so a removal by the first is the only one.
    /// The entry owns `permit` (if any) until it leaves; a refused insert drops it.
    pub(super) fn try_insert(
        &self,
        digest: Digest,
        bytes: Bytes,
        permit: Option<Permit>,
        now: Instant,
        born: u64,
    ) -> bool {
        match self.blobs().entry(digest) {
            std::collections::btree_map::Entry::Occupied(_) => false,
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Entry {
                    bytes,
                    _permit: permit,
                    state: DrainState::Queued,
                    since: now,
                    born,
                });
                true
            }
        }
    }

    /// Drops `digest`, and with it the budget it held; whether it was held. Called only
    /// once the index has the blob.
    pub(super) fn remove(&self, digest: &Digest) -> bool {
        let removed = self.blobs().remove(digest).is_some();
        if removed {
            self.left.notify_waiters();
        }
        removed
    }

    /// The oldest generation a held blob was acknowledged under, if any is held.
    pub(super) fn oldest_born(&self) -> Option<u64> {
        self.blobs().values().map(|e| e.born).min()
    }

    /// The generation `digest` was acknowledged under, if it is held.
    pub(super) fn born_of(&self, digest: &Digest) -> Option<u64> {
        self.blobs().get(digest).map(|e| e.born)
    }

    /// Whether nothing is held.
    pub(super) fn is_empty(&self) -> bool {
        self.blobs().is_empty()
    }

    /// Whether any of `digests` is held.
    fn any(&self, digests: &[Digest]) -> bool {
        let blobs = self.blobs();
        !blobs.is_empty() && digests.iter().any(|d| blobs.contains_key(d))
    }

    /// The bytes held under `digest`.
    pub(super) fn get(&self, digest: &Digest) -> Option<Bytes> {
        self.blobs().get(digest).map(|e| e.bytes.clone())
    }

    /// Whether `digest` is held.
    pub(super) fn has(&self, digest: &Digest) -> bool {
        self.blobs().contains_key(digest)
    }

    /// `digests` without the held ones: all of them, borrowed, while nothing is held.
    pub(super) fn unheld<'a>(&self, digests: &'a [Digest]) -> Cow<'a, [Digest]> {
        let blobs = self.blobs();
        if blobs.is_empty() {
            return Cow::Borrowed(digests);
        }
        Cow::Owned(
            digests
                .iter()
                .filter(|d| !blobs.contains_key(d))
                .copied()
                .collect(),
        )
    }

    /// The held blobs among `digests` (one entry per distinct digest, with its bytes) and
    /// the rest, in order, from a single look at the set: a blob the drain moves is in
    /// one part or the other, never neither. The rest is `digests` itself, borrowed,
    /// while nothing is held.
    pub(super) fn split<'a>(
        &self,
        digests: &'a [Digest],
    ) -> (Vec<(Digest, Bytes)>, Cow<'a, [Digest]>) {
        let blobs = self.blobs();
        if blobs.is_empty() {
            return (Vec::new(), Cow::Borrowed(digests));
        }
        let mut held = BTreeMap::new();
        let mut unheld = Vec::new();
        for d in digests {
            match blobs.get(d) {
                Some(entry) => {
                    held.insert(*d, entry.bytes.clone());
                }
                None => unheld.push(*d),
            }
        }
        (held.into_iter().collect(), Cow::Owned(unheld))
    }

    /// Which of `digests` are held, in order.
    pub(super) fn which(&self, digests: &[Digest]) -> Vec<bool> {
        let blobs = self.blobs();
        digests.iter().map(|d| blobs.contains_key(d)).collect()
    }

    /// The bytes and blobs held and the age of the oldest, as of `now`.
    pub(super) fn unwritten(&self, now: Instant) -> Unwritten {
        let blobs = self.blobs();
        Unwritten {
            bytes: blobs.values().map(|e| e.bytes.len() as u64).sum(),
            blobs: blobs.len() as u64,
            oldest: blobs
                .values()
                .map(|e| now.saturating_duration_since(e.since))
                .max()
                .unwrap_or_default(),
        }
    }

    /// Takes at most `limit` blobs for a drain pass, marking each `Writing` so no other
    /// pass takes it: the queued ones, and a failed one whose wait has ended at `now`
    /// (every failed one with `force`, which a shutdown drain uses: it does not wait out
    /// a backoff).
    pub(super) fn claim(&self, now: Instant, force: bool, limit: usize) -> Vec<(Digest, Bytes)> {
        let mut blobs = self.blobs();
        let mut taken = Vec::new();
        for (digest, entry) in blobs.iter_mut() {
            if taken.len() == limit {
                break;
            }
            let failures = match entry.state {
                DrainState::Queued => 0,
                DrainState::Failing { n, next } if force || next <= now => n,
                DrainState::Failing { .. } | DrainState::Writing { .. } => continue,
            };
            entry.state = DrainState::Writing { failures };
            taken.push((*digest, entry.bytes.clone()));
        }
        taken
    }

    /// Records that the pass that had `digest` failed: the failures in a row, and when the
    /// next pass may take it (`None` if it is not held, or no pass has it).
    pub(super) fn failed(&self, digest: &Digest, now: Instant) -> Option<(u32, Instant)> {
        let mut blobs = self.blobs();
        let entry = blobs.get_mut(digest)?;
        let before = match entry.state {
            DrainState::Writing { failures } => failures,
            DrainState::Queued | DrainState::Failing { .. } => 0,
        };
        let n = before.saturating_add(1);
        let next = now + super::flush::flush_backoff(n);
        entry.state = DrainState::Failing { n, next };
        Some((n, next))
    }

    /// Gives back `digest` to the queue if a pass took it and ended without an answer (it
    /// was cancelled): it is taken again, with the failures it had.
    pub(super) fn unclaim(&self, digest: &Digest, now: Instant) {
        if let Some(entry) = self.blobs().get_mut(digest)
            && let DrainState::Writing { failures } = entry.state
        {
            entry.state = if failures == 0 {
                DrainState::Queued
            } else {
                DrainState::Failing {
                    n: failures,
                    next: now,
                }
            };
        }
    }

    /// The state of `digest`'s drain, if it is held.
    #[cfg(any(test, feature = "testkit"))]
    pub(super) fn state(&self, digest: &Digest) -> Option<DrainState> {
        self.blobs().get(digest).map(|e| e.state)
    }

    /// The earliest a failed blob is due for another pass, if any is waiting.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.blobs()
            .values()
            .filter_map(|e| match e.state {
                DrainState::Failing { next, .. } => Some(next),
                _ => None,
            })
            .min()
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Drops the held blob `digest`, as the drain does once the index has it; whether it
    /// was held. Only for tests.
    #[cfg(feature = "testkit")]
    pub fn release_for_test(&self, digest: &Digest) -> bool {
        self.held.remove(digest)
    }

    /// The same cache, awaiting `hook` after a reader's Held step and before its index
    /// step, so a test can run the drain (the index takes the blob, the set drops it)
    /// between the two. Only with the `testkit` feature. Set it before the cache serves a
    /// read; a second call keeps the first hook.
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn with_between_hook(self, hook: BetweenHook) -> Self {
        let _ = self.between.set(BetweenCell(hook));
        self
    }

    /// The point between a reader's Held step and its index step.
    #[cfg(feature = "testkit")]
    pub(super) async fn between_steps(&self) {
        if let Some(hook) = self.between.get() {
            (hook.0)().await;
        }
    }

    /// The point between a reader's Held step and its index step: nothing in a release
    /// build.
    #[cfg(not(feature = "testkit"))]
    #[expect(clippy::unused_async, reason = "the testkit build awaits a hook here")]
    pub(super) async fn between_steps(&self) {}

    /// Answers which of `digests` are durable, in order: committed to the index at a
    /// reachable object, as `FindMissingBlobs` with `kbf-durable: only` asks
    /// ([`kbf_storage::Storage::find_durable`]). A blob held in RAM and not yet in the
    /// index is missing: a restart would lose it. The empty blob is never missing.
    /// The digests it reports present (the indexed ones) are touched exactly as
    /// [`Cache::find_missing`] touches them, so an action-cache hit or a leg dedup that
    /// asks this keeps the blobs of the entry alive on their owners; a held-only or absent
    /// digest is missing and is not touched.
    ///
    /// # Errors
    /// The metadata log is unavailable; never a partial answer.
    pub async fn durable(&self, digests: &[Digest]) -> Result<Vec<Digest>, CacheError> {
        let (missing, objects) = self
            .touched(|s| {
                let found = s.find_missing(digests);
                ((found.missing, found.objects), found.touch)
            })
            .await?;
        self.used(objects);
        Ok(missing
            .into_iter()
            .filter(|d| !super::is_empty_blob(d))
            .collect())
    }

    /// [`Cache::durable`] after waiting, for at most [`Cache::with_durable_wait`], for the
    /// held digests among `digests` to leave the set (`kbf-durable: wait`
    /// ([`kbf_storage::Storage::find_durable_waiting`])). A digest still held when the
    /// bound ends is missing, because the answer comes from the index alone. Once present,
    /// a digest is touched, as in [`Cache::durable`].
    ///
    /// # Errors
    /// As [`Cache::durable`].
    pub async fn durable_waiting(&self, digests: &[Digest]) -> Result<Vec<Digest>, CacheError> {
        self.wait_drained(digests).await;
        self.durable(digests).await
    }

    /// Returns when none of `digests` is held, or [`Cache::with_durable_wait`] after it
    /// was called, whichever comes first; at once when none is held (always, on a node
    /// that does not acknowledge from RAM). A blob leaves the set only after the index has
    /// it, so a caller that asks the index afterwards finds what has left.
    pub(super) async fn wait_drained(&self, digests: &[Digest]) {
        if !self.held.any(digests) {
            return;
        }
        let deadline = tokio::time::Instant::now() + self.durable_wait;
        loop {
            // Registered before the set is looked at again, so an entry that leaves in
            // between wakes this wait instead of being missed.
            let left = self.held.left.notified();
            tokio::pin!(left);
            left.as_mut().enable();
            if !self.held.any(digests) {
                return;
            }
            self.parked();
            if tokio::time::timeout_at(deadline, left).await.is_err() {
                return;
            }
        }
    }

    /// [`Cache::wait_drained`] for the blobs of an action-cache entry: `result` if given,
    /// and `closure`. The AC owner's barrier (`docs/design/storage-ram-ack.md`, rule 2,
    /// step b): the entry is checked against the index, so a blob this node holds only in
    /// RAM must have been written first. No allocation while nothing is held.
    pub(super) async fn wait_for_entry(&self, result: Option<&Digest>, closure: &Closure) {
        if self.held.is_empty() {
            return;
        }
        let digests: Vec<Digest> = result.into_iter().chain(closure.iter()).copied().collect();
        self.wait_drained(&digests).await;
    }

    /// The same cache, waiting at most `wait` for held blobs to be written where a caller
    /// asks it to ([`Cache::durable_waiting`], and the action-cache writes, which wait for
    /// the blobs of the entry): `--durable-wait`, [`DEFAULT_DURABLE_WAIT`] unless set.
    #[must_use]
    pub fn with_durable_wait(mut self, wait: Duration) -> Self {
        self.durable_wait = wait;
        self
    }

    /// Notes that a wait is parked, for [`Cache::parked_for_test`].
    #[cfg(feature = "testkit")]
    fn parked(&self) {
        self.parks.send_modify(|n| *n += 1);
    }

    /// Notes that a wait is parked: nothing in a release build.
    #[cfg(not(feature = "testkit"))]
    #[expect(clippy::unused_self, reason = "the testkit build counts the wait here")]
    fn parked(&self) {}

    /// Returns once `count` waits for held blobs have parked in all (each time a wait
    /// finds a blob of its digests still held and sleeps until one leaves or its bound
    /// ends). A positive event for a test to wait on, so it never polls or sleeps. Only
    /// with the `testkit` feature.
    #[cfg(feature = "testkit")]
    pub async fn parked_for_test(&self, count: usize) {
        let mut parks = self.parks.subscribe();
        // The sender lives in `self`, so the wait ends in the count.
        let _ = parks.wait_for(|n| *n >= count).await;
    }
}
