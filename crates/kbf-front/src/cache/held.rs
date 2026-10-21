//! The blobs a node holds in RAM and has acknowledged but not yet written to its disk
//! (`docs/design/storage-ram-ack.md`), and the order every reader consults them in.
//!
//! **Nothing puts a blob here yet.** A later change registers a blob when a write is
//! acknowledged from RAM and removes it once the blob is in the index; until then the set
//! is empty and every reader answers from the index alone, as before. A test puts blobs in
//! through [`Cache::hold_for_test`] (the `testkit` feature).
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

use bytes::Bytes;
use kbf_meta::Closure;
use kbf_types::Digest;

#[cfg(feature = "testkit")]
use super::VerifiedBlob;
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

/// The set: digest to the verified bytes.
#[derive(Debug, Default)]
pub(super) struct Held {
    blobs: Mutex<BTreeMap<Digest, Bytes>>,
    /// Woken when an entry has left, for a caller waiting for digests to be written.
    left: tokio::sync::Notify,
}

impl Held {
    fn blobs(&self) -> MutexGuard<'_, BTreeMap<Digest, Bytes>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds `bytes` under `digest` unless the digest is held already; whether it did. A
    /// second writer of a digest is a dedup answer: it takes no entry and leaves the first
    /// writer's, so a removal by the first is the only one.
    #[cfg(any(test, feature = "testkit"))]
    pub(super) fn try_insert(&self, digest: Digest, bytes: Bytes) -> bool {
        match self.blobs().entry(digest) {
            std::collections::btree_map::Entry::Occupied(_) => false,
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(bytes);
                true
            }
        }
    }

    /// Drops `digest`; whether it was held.
    #[cfg(any(test, feature = "testkit"))]
    pub(super) fn remove(&self, digest: &Digest) -> bool {
        let removed = self.blobs().remove(digest).is_some();
        if removed {
            self.left.notify_waiters();
        }
        removed
    }

    /// Whether nothing is held.
    fn is_empty(&self) -> bool {
        self.blobs().is_empty()
    }

    /// Whether any of `digests` is held.
    fn any(&self, digests: &[Digest]) -> bool {
        let blobs = self.blobs();
        !blobs.is_empty() && digests.iter().any(|d| blobs.contains_key(d))
    }

    /// The bytes held under `digest`.
    pub(super) fn get(&self, digest: &Digest) -> Option<Bytes> {
        self.blobs().get(digest).cloned()
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
                Some(bytes) => {
                    held.insert(*d, bytes.clone());
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
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Holds `blob` as an acknowledged blob not yet on disk would be held, and whether it
    /// did (`false`: the digest was held already, and the set is unchanged). Only for
    /// tests: nothing holds a blob in a running node yet.
    #[cfg(feature = "testkit")]
    pub fn hold_for_test(&self, blob: &VerifiedBlob) -> bool {
        self.held.try_insert(blob.digest, blob.bytes.clone())
    }

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
    /// was called, whichever comes first; at once when none is held (always, while
    /// nothing puts a blob in the set). A blob leaves the set only after the index has
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
