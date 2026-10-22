//! What a storage node does with the acknowledged data it has not yet put at its final
//! place: the drain that writes the blobs it holds in RAM (`--ram-ack-bytes`) to its disk
//! while it serves, and, when it stops, the bounded drain of those blobs and then the
//! flush of the blocks staged on its hot store to the cold store
//! (`docs/design/storage-ram-ack.md`, "Drain and shutdown").
//!
//! With `--ram-ack-bytes` at 0 nothing is held (the acknowledgement point holds no blob at
//! 0): no task is started and a stop drains nothing from RAM, so the flag's default changes
//! no behaviour. A stop still flushes what is staged on the hot store, with or without a
//! budget.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MetaLog, ShutdownDrain, ShutdownFlush};
use kbf_objstore::ObjectStore;
use tokio::time::Instant;

use crate::start::AbortOnDrop;

/// The drain of one node's cache: a background task while the node serves, and
/// [`RamDrain::after`] for its stop.
pub struct RamDrain<M, O> {
    cache: Arc<Cache<M, O>>,
    /// `--ram-drain-deadline`: all that a stop waits, for the blobs held in RAM and then
    /// for the staged blocks.
    deadline: Duration,
    /// Whether the cache holds blobs in RAM (`--ram-ack-bytes` over 0).
    holds: bool,
    _task: Option<AbortOnDrop>,
}

impl<M: MetaLog + 'static, O: ObjectStore + 'static> RamDrain<M, O> {
    /// The drain of `cache`, which has `budget` bytes of RAM to hold (`--ram-ack-bytes`)
    /// and whose stop waits for `deadline` at most: with a budget of 0, no task is
    /// started and nothing is drained from RAM; with one, a task writes held blobs to the
    /// disk until the returned value is dropped.
    #[must_use]
    pub fn start(cache: Arc<Cache<M, O>>, budget: u64, deadline: Duration) -> Self {
        let holds = budget > 0;
        let task = holds.then(|| {
            let runner = Arc::clone(&cache);
            tokio::spawn(async move { match runner.run_drain().await {} })
        });
        Self {
            cache,
            deadline,
            holds,
            _task: task.map(AbortOnDrop),
        }
    }

    /// Serves as `served` does and, once it has ended, writes the blobs still held in RAM
    /// to the disk and then flushes the blocks staged on the hot store to the cold store,
    /// both within the deadline; the drain's task stops with it. A node that serves calls
    /// on `served` has none left by then, so nothing is acknowledged meanwhile. What the
    /// deadline leaves is logged: blobs held in RAM are lost to the stop, and blocks
    /// staged on the hot store are on its disk, flushed by the next start.
    pub async fn after<F: Future>(self, served: F) -> F::Output {
        let out = served.await;
        let stopped = Instant::now();
        if self.holds {
            match self.cache.drain_for_shutdown(self.deadline).await {
                ShutdownDrain::Drained => {
                    tracing::info!("the blobs held in RAM are written to disk");
                }
                ShutdownDrain::TimedOut { blobs, bytes } => {
                    tracing::error!(
                        blobs,
                        bytes,
                        deadline = ?self.deadline,
                        "--ram-drain-deadline ended with blobs still held in RAM; a restart loses them"
                    );
                }
            }
        }
        let left = self.deadline.saturating_sub(stopped.elapsed());
        match self.cache.flush_for_shutdown(left).await {
            ShutdownFlush::Flushed => {
                tracing::info!("no block is left staged on the hot store");
            }
            ShutdownFlush::TimedOut { blocks, bytes } => {
                tracing::error!(
                    blocks,
                    bytes,
                    deadline = ?self.deadline,
                    "--ram-drain-deadline ended with blocks still staged on the hot store; \
                     the next start flushes them"
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
