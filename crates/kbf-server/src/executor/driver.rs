//! The task that keeps an [`Executor`]'s place in the state store while the frontend serves:
//! it renews the process's liveness row every quarter of [`Config::live`], and every half of it
//! takes up the unfinished operations of processes whose liveness row has expired
//! ([`Rows::orphans`]). On [`Running::stop`] the liveness row is deleted, so the frontends that
//! go on take up at once what this process leaves. An executor with no state store has nothing
//! to keep, and its task only waits for the stop.
//!
//! [`Config::live`]: crate::executor::Config::live
//! [`Rows::orphans`]: crate::executor::rows::Rows::orphans

use std::sync::Arc;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use crate::executor::run::{self, Op, Start};
use crate::executor::{Executor, Inner};

/// An executor's task beside the frontend. Dropped without [`Running::stop`], it ends its task:
/// a server whose serving future is dropped leaves its liveness row to expire.
pub struct Running {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
    /// Ends the task when the handle is dropped unstopped.
    _abort: crate::serve::AbortOnDrop,
}

impl Running {
    /// Keeps `executor`'s place in the state store until [`stop`](Self::stop).
    #[must_use]
    pub fn spawn(executor: &Executor) -> Self {
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(drive(Arc::clone(executor.inner()), stopped));
        let abort = crate::serve::AbortOnDrop(task.abort_handle());
        Self {
            stop,
            task,
            _abort: abort,
        }
    }

    /// Stops the task and deletes the liveness row. Nothing is cancelled: an operation still
    /// open is taken up by another frontend of the store.
    pub async fn stop(self) {
        let Self { stop, task, _abort } = self;
        let _ = stop.send(true);
        if let Err(e) = task.await {
            tracing::error!(error = %e, "the executor's task ended abnormally");
        }
    }
}

async fn drive(inner: Arc<Inner>, mut stopped: watch::Receiver<bool>) {
    if inner.rows.is_none() {
        let _ = stopped.wait_for(|stop| *stop).await;
        return;
    }
    let beat = inner.cfg.live / 4;
    let sweep = inner.cfg.live / 2;
    let mut swept = Instant::now();
    keep(&inner, true).await;
    loop {
        tokio::select! {
            _ = stopped.wait_for(|stop| *stop) => break,
            () = sleep(beat) => {}
        }
        let due = swept.elapsed() >= sweep;
        if due {
            swept = Instant::now();
        }
        keep(&inner, due).await;
    }
    let inner_for_leave = Arc::clone(&inner);
    let left =
        tokio::task::spawn_blocking(move || inner_for_leave.rows.as_ref().map(|rows| rows.leave()))
            .await;
    if let Ok(Some(Err(error))) = left {
        tracing::warn!(%error, "the liveness row could not be deleted: it expires on its own");
    }
}

/// Renews the liveness row and, when `sweep`, takes up the operations of gone processes.
async fn keep(inner: &Arc<Inner>, sweep: bool) {
    let live = inner.cfg.live;
    let held = Arc::clone(inner);
    let swept = tokio::task::spawn_blocking(move || {
        let rows = held.rows.as_ref()?;
        if let Err(error) = rows.beat(live) {
            tracing::warn!(%error, "the liveness row could not be renewed");
        }
        if !sweep {
            return Some(Vec::new());
        }
        let orphans = match rows.orphans() {
            Ok(orphans) => orphans,
            Err(error) => {
                tracing::warn!(%error, "the operations of gone frontends could not be read");
                return Some(Vec::new());
            }
        };
        let claimed = orphans.into_iter().filter_map(|orphan| match rows.claim(&orphan) {
            Ok(row) => Some((orphan.operation, row)),
            Err(error) => {
                tracing::info!(operation = %orphan.operation, %error, "another frontend took the operation up");
                None
            }
        });
        Some(claimed.collect())
    })
    .await;
    let Ok(Some(claimed)) = swept else {
        return;
    };
    for (operation, row) in claimed {
        let key = match row.action_key() {
            Ok(key) => key,
            Err(why) => {
                tracing::warn!(%operation, %why, "an operation row names no action: it is left");
                continue;
            }
        };
        let cancel = {
            let mut ops = inner.ops();
            if ops.is_open(operation) {
                continue;
            }
            ops.open(operation, key.clone()).1
        };
        inner.deps.metrics.reattached();
        let op = Op::new(operation, key, row.whole_machine, false);
        tokio::spawn(run::run(
            Arc::clone(inner),
            op,
            Start::Attached(row),
            cancel,
        ));
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
