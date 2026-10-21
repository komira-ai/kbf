//! A node that holds blobs in RAM (`--ram-ack-bytes`): the drain that writes them to its
//! disk while it serves, and the bounded drain when it stops
//! (`docs/design/storage-ram-ack.md`, "Drain and shutdown").
//!
//! With `--ram-ack-bytes` at 0 nothing is held: no task is started and a stop drains
//! nothing. Nothing holds a blob in a running node yet either way, so this changes no
//! behaviour until the acknowledgement point lands.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MetaLog, ShutdownDrain};
use kbf_objstore::ObjectStore;

use crate::start::AbortOnDrop;

/// The drain of one node's cache: a background task while the node serves, and
/// [`RamDrain::after`] for its stop.
pub struct RamDrain<M, O> {
    cache: Arc<Cache<M, O>>,
    /// `--ram-drain-deadline`; `None` with no budget (nothing is held, nothing is drained).
    deadline: Option<Duration>,
    _task: Option<AbortOnDrop>,
}

impl<M: MetaLog + 'static, O: ObjectStore + 'static> RamDrain<M, O> {
    /// The drain of `cache`, which has `budget` bytes of RAM to hold (`--ram-ack-bytes`):
    /// with 0, nothing at all. With a budget, a task writes held blobs to the disk until
    /// the returned value is dropped.
    #[must_use]
    pub fn start(cache: Arc<Cache<M, O>>, budget: u64, deadline: Duration) -> Self {
        if budget == 0 {
            return Self {
                cache,
                deadline: None,
                _task: None,
            };
        }
        let runner = Arc::clone(&cache);
        let task = tokio::spawn(async move { match runner.run_drain().await {} });
        Self {
            cache,
            deadline: Some(deadline),
            _task: Some(AbortOnDrop(task)),
        }
    }

    /// Serves as `served` does and, once it has ended, writes the blobs still held to the
    /// disk, for at most the deadline; the drain's task stops with it. A node that serves
    /// calls on `served` has none left by then, so nothing is acknowledged meanwhile. What
    /// the deadline leaves held is logged and lost to the stop.
    pub async fn after<F: Future>(self, served: F) -> F::Output {
        let out = served.await;
        if let Some(deadline) = self.deadline {
            match self.cache.drain_for_shutdown(deadline).await {
                ShutdownDrain::Drained => {
                    tracing::info!("the blobs held in RAM are written to disk");
                }
                ShutdownDrain::TimedOut { blobs, bytes } => {
                    tracing::warn!(
                        blobs,
                        bytes,
                        ?deadline,
                        "--ram-drain-deadline ended with blobs still held in RAM; a restart loses them"
                    );
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
