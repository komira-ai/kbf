//! Where the time of one write of an action tree goes, apart from the store's request: what a
//! tree's writes tell their timer ([`WriteTimed`]) beside the time of each stage.
//!
//! One write of a manifest, of the root or of a tree list is timed in four parts
//! ([`WriteWaits`]), each on tokio's clock:
//!
//! - **the manifest slot**: the wait for one of the writer's slots for manifest writes
//!   ([`super::MANIFEST_WRITES`], shared by every tree the frontend writes); a tree list takes
//!   none, so its part is zero;
//! - **the store slot**: the waits for one of the pack store's slots for tree writes
//!   (`crate::store_writes::TREE_WRITES`, shared by every tree the frontend writes, and not with
//!   the pack uploads, which take one of `--pack-store-writes`), one wait per attempt, added up;
//! - **the retry waits** (`store_503`, the name kept from when only a 503 was retried): the waits
//!   between attempts for a store that failed in a way the retry takes (a 503, `SlowDown` or
//!   not, a 5xx, a stalled body, a dead connection: [`super::retry`]), added up;
//! - **the request**: the rest of the write, from its first attempt to its answer, the store
//!   slot's waits and the retry waits taken out: the attempts at the store.
//!
//! The store slot is taken below the writer, in the pack store, which every writer shares. The
//! tree's write runs its attempts under a tally of its own ([`tallied`]), a tokio task-local
//! carried by the write's future; the pack store takes a tree slot for a write under a tally
//! ([`in_tree_write`]), and the slot's wait is added to the tally of the write that waited
//! ([`waited_for_slot`]). A write under no tally (a pack upload, a blob of the tree's first
//! stage, a list's sidecar) takes one of `--pack-store-writes` and adds nothing. The retry waits
//! are added the same way ([`waited_to_retry`]). Tallies are never nested: each write of a
//! manifest, the root or the list is tallied once, by the one place that makes it (the writes in
//! flight, [`super::flight`], and the list's write).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use super::WriteStage;

/// Where the time of one write of an action tree went. The four parts add up to the write's
/// time, from its wait for a manifest slot to its answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteWaits {
    /// The wait for a slot for manifest writes; zero for a tree list, which takes none.
    pub manifest_slot: Duration,
    /// The waits for one of the pack store's slots for tree writes, one per attempt, added up.
    pub store_slot: Duration,
    /// The waits between attempts for a store that failed in a way the retry takes (a 503, a
    /// 5xx, a stalled body, a dead connection), added up.
    pub store_503: Duration,
    /// The attempts at the store: the rest of the write.
    pub request: Duration,
}

/// What an action tree's writes tell their timer, each once it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteTimed {
    /// A stage of the writes took this long, a failed one included.
    Stage(WriteStage, Duration),
    /// One write of a stage that this tree made ended, stored or failed: where its time went.
    /// Only [`WriteStage::Manifests`], [`WriteStage::Root`] and [`WriteStage::List`] have such
    /// writes. A manifest whose write another tree had in flight is that tree's write, and is
    /// told to that tree's timer; a write whose tree was dropped before it ended is told to no
    /// one, even when another tree's wait for it carries it to its end.
    Write(WriteStage, WriteWaits),
    /// The manifests stage stored every manifest it named, this many of them written by this
    /// tree's own requests (`.kbf` among them): not those the memo had, nor those another tree
    /// had in flight. A stage that failed tells none.
    Manifests(u64),
}

/// The waits of one write, added up as they are made.
#[derive(Debug, Default)]
struct Tally {
    store_slot: AtomicU64,
    store_503: AtomicU64,
}

impl Tally {
    fn add(part: &AtomicU64, took: Duration) {
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        // `fetch_add` wraps; a write's waits are far below 584 years of nanoseconds.
        part.fetch_add(nanos, Ordering::Relaxed);
    }

    fn of(part: &AtomicU64) -> Duration {
        Duration::from_nanos(part.load(Ordering::Relaxed))
    }
}

tokio::task_local! {
    /// The tally of the tree's write whose attempts are being made.
    static TALLY: Arc<Tally>;
}

/// Runs `write`, one write's attempts made after a wait of `manifest_slot` for a manifest slot,
/// under a tally of its own, and answers with where its time went.
pub(crate) async fn tallied<T>(
    manifest_slot: Duration,
    write: impl Future<Output = T>,
) -> (T, WriteWaits) {
    let tally = Arc::<Tally>::default();
    let started = Instant::now();
    let answer = TALLY.scope(Arc::clone(&tally), write).await;
    let took = started.elapsed();
    let store_slot = Tally::of(&tally.store_slot);
    let store_503 = Tally::of(&tally.store_503);
    let waits = WriteWaits {
        manifest_slot,
        store_slot,
        store_503,
        request: took.saturating_sub(store_slot).saturating_sub(store_503),
    };
    (answer, waits)
}

/// Whether the write being made is a tree's write of a manifest, its root or its list: made under
/// a tally ([`tallied`]), which only those writes are. The pack store gives such a write one of
/// its slots for tree writes, not one the uploads share (`crate::store_writes`).
pub(crate) fn in_tree_write() -> bool {
    TALLY.try_with(|_| ()).is_ok()
}

/// A write waited `took` for a slot of the pack store: added to its tally, if it has one.
pub(crate) fn waited_for_slot(took: Duration) {
    let _ = TALLY.try_with(|tally| Tally::add(&tally.store_slot, took));
}

/// A write waited `took` before it tried again after a failure the store's retry takes: added
/// to its tally, if it has one.
pub(super) fn waited_to_retry(took: Duration) {
    let _ = TALLY.try_with(|tally| Tally::add(&tally.store_503, took));
}

#[cfg(test)]
#[path = "waits_tests.rs"]
mod tests;
