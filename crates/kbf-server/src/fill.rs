//! A registered storage node's side of the handoff fill (`docs/design/rebalance-handoff.md`,
//! sections 3 and 4): what its heartbeats say of its flush and its tail, and the task that
//! produces it.
//!
//! [`FillState`] is shared between the [`crate::join::Joiner`], which puts it in every
//! heartbeat and records the fence epoch the frontend answers with, and [`run`], which
//! asks the node's cache:
//!
//! * every beat, for its flush report ([`kbf_front::Cache::flush_report`]: the
//!   `flush_clear_gen` and the backlog);
//! * when the frontend's epoch is above what the node acknowledged, and the index rebuild is
//!   done, for a tail pass ([`kbf_front::Cache::tail_pass`]) that begins after the node
//!   learned that epoch. The pass is repeated until one finds nothing it had not handled
//!   ([`kbf_front::TailReport::quiet`], the guard for a listing that lags a put), and only
//!   then is the epoch acknowledged, with the fingerprint of the map the pass began under
//!   (the node assumed `Active`). A map that changed placement meanwhile voids the pass: the
//!   next beat begins it again.
//!
//! The acknowledgement is volatile and goes with the registration: a node that registers
//! again (a restart, or a lapsed row) says no pass is done until it has done one under its
//! new boot, which is what the frontend's fence records.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_front::{Cache, CacheError, FlushReport, MetaLog, TailState};
use kbf_objstore::ObjectStore;
use kbf_objstore::footers::FooterReads;
use kbf_storage::flush::FlushGate;
use kbf_storage::replicate::MapSource as _;
use kbf_storage::service::CatchUp;

use crate::members::fill_fp;
use crate::node_map::NodeMap;

/// Passes of one attempt: the first, the repeat that is the guard, and what a node that is
/// still being written to by others may need before one is quiet.
const PASSES: usize = 8;

/// The flush gate of a registered node: it flushes every staged object, as a node with no
/// gate does, so the flush is what it was; the gate only says which map generation the node
/// holds, by which the cache dates the objects it stages ([`FillState`]'s flush report: with
/// no gate a node cannot say what it has flushed). The one-flusher gate
/// ([`kbf_storage::flush::OneFlusher`]) is not installed: a replica that does not flush
/// keeps its staged copy until an evict-after-confirm that is not built.
#[derive(Debug)]
pub struct FlushesAll(Arc<NodeMap>);

impl FlushesAll {
    /// The gate of a node that holds the maps `node` holds.
    #[must_use]
    pub fn new(node: Arc<NodeMap>) -> Self {
        Self(node)
    }
}

impl FlushGate for FlushesAll {
    fn flushes(&self, _: &kbf_types::Digest) -> bool {
        true
    }

    fn generation(&self) -> Option<u64> {
        self.0.current().map(|map| map.generation())
    }
}

/// What a heartbeat of this node says about its flush and its tail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Beat {
    /// `flush_clear_gen`, `None` when the node cannot say (`flush_known` false).
    pub flush_clear_gen: Option<u64>,
    /// The objects staged and not flushed.
    pub staged_objects: u64,
    /// Their committed bytes.
    pub staged_bytes: u64,
    /// The newest epoch a tail pass was finished for (0: none).
    pub tail_epoch_done: u64,
    /// The fingerprint of the map that pass began under (0: none).
    pub fill_fp: u64,
}

#[derive(Debug, Default)]
struct Inner {
    flush: Option<FlushReport>,
    /// The newest epoch the frontend answered with.
    learned: u64,
    done: u64,
    done_fp: u64,
}

/// The fill a node reports, shared between its heartbeats and [`run`].
#[derive(Debug, Default)]
pub struct FillState(Mutex<Inner>);

impl FillState {
    /// A node that has reported nothing.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The node's flush report now (`None`: the cache could not answer). [`round`] makes it;
    /// a test says it.
    pub fn flushed(&self, report: Option<FlushReport>) {
        self.inner().flush = report;
    }

    /// The epoch the frontend's answer carried. It is followed, not maximised: a frontend
    /// whose state was reset starts counting again, and an acknowledgement above its
    /// epoch is not one ([`FillState::beat`]).
    pub(crate) fn learn(&self, epoch: u64) {
        self.inner().learned = epoch;
    }

    /// The epoch a pass is owed for: the learned one, if above what was acknowledged.
    pub fn owed(&self) -> Option<u64> {
        let inner = self.inner();
        (inner.learned > inner.done).then_some(inner.learned)
    }

    /// A pass that began after `epoch` was learned, under a map of fingerprint `fp`, is done.
    pub fn finish(&self, epoch: u64, fp: u64) {
        let mut inner = self.inner();
        if epoch > inner.done {
            inner.done = epoch;
            inner.done_fp = fp;
        }
    }

    /// A registration: its boot has done no pass.
    pub(crate) fn registered(&self) {
        let mut inner = self.inner();
        inner.done = 0;
        inner.done_fp = 0;
    }

    /// What the next heartbeat says.
    #[must_use]
    pub fn beat(&self) -> Beat {
        let inner = self.inner();
        // An acknowledgement above the frontend's epoch is a leftover of another count.
        let (done, fp) = if inner.done <= inner.learned {
            (inner.done, inner.done_fp)
        } else {
            (0, 0)
        };
        let flush = inner.flush.unwrap_or_default();
        Beat {
            flush_clear_gen: inner.flush.and(flush.clear_gen),
            staged_objects: flush.staged_objects,
            staged_bytes: flush.staged_bytes,
            tail_epoch_done: done,
            fill_fp: fp,
        }
    }
}

/// The fingerprint of the map `node` holds with this node assumed `Active`; `None` before
/// it has a map or knows which member it is.
fn own_fp(node: &NodeMap) -> Option<u64> {
    fill_fp(node.current()?.as_ref(), node.me()?)
}

/// Tail passes until one is quiet after the first; whether one was.
async fn settle<M: MetaLog, O: ObjectStore>(
    cache: &Cache<M, O>,
    tail: &mut TailState,
    reads: FooterReads,
) -> Result<bool, CacheError> {
    for pass in 1..=PASSES {
        let report = cache.tail_pass(tail, reads).await?;
        if pass > 1 && report.quiet() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One round: reports the flush, and if a tail pass is owed and the node can make it,
/// makes it and acknowledges the epoch.
pub(crate) async fn round<M: MetaLog, O: ObjectStore>(
    cache: &Cache<M, O>,
    fill: &FillState,
    node: &NodeMap,
    catch_up: &CatchUp,
    tail: &mut TailState,
    reads: FooterReads,
) {
    match cache.flush_report().await {
        Ok(report) => fill.flushed(Some(report)),
        Err(error) => {
            tracing::warn!(%error, "the flush report failed; this node says it cannot say");
            fill.flushed(None);
        }
    }
    // A pass over a half-built index would acknowledge objects the rebuild has yet to adopt.
    if catch_up.is_catching_up() {
        return;
    }
    let (Some(epoch), Some(fp)) = (fill.owed(), own_fp(node)) else {
        return;
    };
    match settle(cache, tail, reads).await {
        // The placement moved under the pass: it proves nothing; the next round begins it.
        Ok(true) if own_fp(node) == Some(fp) => {
            tracing::info!(epoch, "tail pass done");
            fill.finish(epoch, fp);
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "tail pass failed; trying again"),
    }
}

/// [`round`] every `every`, for ever.
pub(crate) async fn run<M: MetaLog, O: ObjectStore>(
    cache: Arc<Cache<M, O>>,
    fill: Arc<FillState>,
    node: Arc<NodeMap>,
    catch_up: CatchUp,
    every: Duration,
) {
    let mut tail = TailState::default();
    let reads = FooterReads::default();
    let mut tick = tokio::time::interval(every.max(Duration::from_millis(10)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        round(&cache, &fill, &node, &catch_up, &mut tail, reads).await;
    }
}

#[cfg(test)]
#[path = "fill_tests.rs"]
mod tests;
