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

use bytes::Bytes;
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

/// The set: digest to the verified bytes.
#[derive(Debug, Default)]
pub(super) struct Held {
    blobs: Mutex<BTreeMap<Digest, Bytes>>,
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
        self.blobs().remove(digest).is_some()
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
    /// Touches nothing: it is a question asked before something durable is written, not
    /// a use of the blobs.
    ///
    /// # Errors
    /// The metadata log is unavailable; never a partial answer.
    pub async fn durable(&self, digests: &[Digest]) -> Result<Vec<Digest>, CacheError> {
        let missing = self
            .writer
            .query(|s| s.find_missing(digests).missing)
            .await?;
        Ok(missing
            .into_iter()
            .filter(|d| !super::is_empty_blob(d))
            .collect())
    }
}
