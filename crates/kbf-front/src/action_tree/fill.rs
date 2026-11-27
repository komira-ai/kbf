//! The fill: a tree list dual emission left with no L2 sidecar gets one soon after its write.
//!
//! Dual emission ([`Sidecars::emit`]) writes a list's sidecar from recorded cells only, and reads
//! no object, so that the list's write waits for no read. A list naming an object whose cells no
//! writer recorded (the base tree's objects before they were, a large blob a client uploaded, a
//! whole object a tree found in the store, a pack written before cells were recorded or adopted)
//! is recorded unlisted and, with fills ([`Sidecars::with_fills`]), queued here. One task per
//! [`Sidecars`] (and its clones) runs the miss path's backfill ([`Sidecars::backfill`], whose
//! failure is counted here and not in `kbf_l2_sidecars_total`) on the queued lists, one at a time
//! in the order they were queued: each object a list names that has no cells is read once and
//! its cells recorded, the sidecar is written and the tree's unlisted row dropped. Since cells
//! are recorded by object, not by list, the next list naming the same objects gets its sidecar
//! at its write, and reads nothing.
//!
//! The queue holds the bytes of each list (a list is not read again from the store), each tree
//! once, at most the limit [`Sidecars::with_fills`] was given in all: a list that would take it
//! over is dropped and counted, and keeps its unlisted row for the operator's backfill
//! (`kbf-l2-backfill`). The task ends when the queue is empty and the next queued list starts
//! another. A fill that fails (an object the store lost, a store or index error) is counted and
//! logged; the list keeps its unlisted row. Nothing here fails or delays a list's write.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use tokio::sync::Notify;

use super::sidecar::{SidecarError, Sidecars};

/// The bytes of tree lists a frontend's fill queue holds at most ([`Sidecars::with_fills`]).
pub const FILL_QUEUE_BYTES: u64 = 256 << 20;

/// The lists queued for a fill, and whether a task is running them.
pub(crate) struct Fills {
    limit: u64,
    state: Mutex<Queue>,
    /// Told when the task ends, the queue empty.
    idle: Notify,
}

#[derive(Default)]
struct Queue {
    /// The lists waiting, oldest first: the tree id and the list's bytes.
    waiting: VecDeque<([u8; 32], Bytes)>,
    /// The trees waiting or being filled.
    queued: BTreeSet<[u8; 32]>,
    /// The bytes of the lists waiting or being filled.
    bytes: u64,
    /// A task is running the queue.
    running: bool,
}

impl Fills {
    /// A queue of lists holding at most `limit` bytes.
    pub(super) fn new(limit: u64) -> Self {
        Self {
            limit,
            state: Mutex::default(),
            idle: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Sidecars {
    /// Queues the list `v2` of the tree `tree` for a fill, if these sidecars fill, and starts
    /// the task that runs the queue if none is running. A tree already queued is not queued
    /// again; a list that would take the queue over its limit is dropped and counted.
    pub(super) fn queue_fill(&self, tree: [u8; 32], v2: &[u8]) {
        let Some(fills) = &self.fills else {
            return;
        };
        let len = v2.len() as u64;
        let start = {
            let mut queue = fills.lock();
            if queue.queued.contains(&tree) {
                return;
            }
            if queue.bytes.saturating_add(len) > fills.limit {
                self.counters.fills.dropped.add(1);
                tracing::debug!("the fill queue is full: the tree list is left to the backfill");
                return;
            }
            queue.queued.insert(tree);
            queue.bytes += len;
            queue.waiting.push_back((tree, Bytes::copy_from_slice(v2)));
            !std::mem::replace(&mut queue.running, true)
        };
        if start {
            tokio::spawn(self.clone().run_fills(Arc::clone(fills)));
        }
    }

    /// Fills the queued lists one at a time until the queue is empty.
    async fn run_fills(self, fills: Arc<Fills>) {
        loop {
            let next = {
                let mut queue = fills.lock();
                let next = queue.waiting.pop_front();
                if next.is_none() {
                    queue.running = false;
                }
                next
            };
            let Some((tree, v2)) = next else {
                fills.idle.notify_waiters();
                return;
            };
            self.fill(&v2).await;
            let mut queue = fills.lock();
            queue.queued.remove(&tree);
            queue.bytes -= v2.len() as u64;
        }
    }

    /// One fill: the miss path's backfill of `v2`, counted by how it ended.
    async fn fill(&self, v2: &[u8]) {
        let fills = &self.counters.fills;
        match self.backfill_counted(v2).await {
            Ok(done) => {
                fills.written.add(1);
                tracing::debug!(?done, "a tree list's L2 sidecar was filled");
            }
            Err(error) => {
                match error {
                    SidecarError::Lost { .. } => fills.lost.add(1),
                    _ => fills.failed.add(1),
                }
                tracing::warn!(%error, "a tree list's L2 sidecar was not filled: it is left to the backfill");
            }
        }
    }

    /// Waits until no fill is queued or running. With no fills, at once.
    pub async fn settled(&self) {
        let Some(fills) = &self.fills else {
            return;
        };
        loop {
            let idle = fills.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if !fills.lock().running {
                return;
            }
            idle.await;
        }
    }
}
