//! The task that steps an [`Executor`] while the frontend serves, and stops it when the
//! frontend does.
//!
//! The executor is stepped every [`Executor::poll`], a pass run to its end. Before the first
//! pass, and every [`Executor::rival_check`] after, it asks the lease server whether another
//! frontend holds hosts: the executor runs on exactly one frontend, and a second one stops, its
//! hosts released, and says so ([`Running::failed`]). On [`Running::stop`] the farm is told
//! each worker takes no more work, the runs are stopped and every lease is released
//! ([`Executor::shutdown`]).

use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use crate::executor::Executor;

/// An executor stepping in a task of its own.
pub struct Running {
    stop: watch::Sender<bool>,
    failed: oneshot::Receiver<String>,
    task: JoinHandle<()>,
}

impl Running {
    /// Steps `executor` until [`stop`](Self::stop), or until another frontend is found to hold
    /// hosts.
    #[must_use]
    pub fn spawn(executor: Executor) -> Self {
        let (stop, stopped) = watch::channel(false);
        let (fail, failed) = oneshot::channel();
        let task = tokio::spawn(drive(executor, stopped, fail));
        Self { stop, failed, task }
    }

    /// Why the executor stopped by itself: completes if it did, never while it runs on.
    pub async fn failed(&mut self) -> String {
        match (&mut self.failed).await {
            Ok(why) => why,
            // The task ended with no reason given: it broke.
            Err(_) => "the executor's task ended without a reason".to_owned(),
        }
    }

    /// Stops the executor and waits until its leases are released.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        if let Err(e) = self.task.await {
            tracing::error!(error = %e, "the executor's task ended abnormally");
        }
    }
}

async fn drive(
    mut executor: Executor,
    mut stopped: watch::Receiver<bool>,
    failed: oneshot::Sender<String>,
) {
    let mut next_check = Instant::now();
    loop {
        if Instant::now() >= next_check {
            next_check = Instant::now() + executor.rival_check();
            if let Some(why) = executor.rival().await {
                tracing::error!(%why, "another frontend holds hosts: the executor runs on exactly one frontend");
                let _ = failed.send(why);
                break;
            }
        }
        executor.step().await;
        if let Some(why) = executor.twin() {
            tracing::error!(%why, "another process runs under this frontend's id");
            let _ = failed.send(why.to_owned());
            break;
        }
        tokio::select! {
            _ = stopped.changed() => break,
            () = sleep(executor.poll()) => {}
        }
    }
    executor.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(task: JoinHandle<()>) -> (Running, oneshot::Sender<String>) {
        let (stop, _stopped) = watch::channel(false);
        let (fail, failed) = oneshot::channel();
        (Running { stop, failed, task }, fail)
    }

    /// Catches a task that ends with nothing said being waited on for ever: the frontend that
    /// is told it ended stops with an error instead of serving without its executor.
    #[tokio::test]
    async fn a_task_that_ended_without_a_reason_is_told_as_such() {
        let (mut running, fail) = running(tokio::spawn(async {}));
        drop(fail);
        assert_eq!(
            running.failed().await,
            "the executor's task ended without a reason"
        );
        running.stop().await;
    }

    /// Catches a task that broke taking the stop down with it: the stop returns, and the break
    /// is in the log.
    #[tokio::test]
    async fn a_task_that_broke_does_not_stop_the_stop() {
        let (running, _fail) = running(tokio::spawn(async { panic!("the executor broke") }));
        running.stop().await;
    }
}
