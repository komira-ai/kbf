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
    failed: Option<oneshot::Receiver<String>>,
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
        Self {
            stop,
            failed: Some(failed),
            task,
        }
    }

    /// Why the executor stopped by itself: completes if it did, never if it runs on or was
    /// stopped.
    pub async fn failed(&mut self) -> String {
        match self.failed.as_mut() {
            Some(failed) => match failed.await {
                Ok(why) => why,
                Err(_) => std::future::pending().await,
            },
            None => std::future::pending().await,
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
            if let Some(rival) = executor.rival().await {
                tracing::error!(
                    %rival,
                    "another frontend holds hosts on the lease server: the executor runs on exactly one frontend"
                );
                let _ = failed.send(format!(
                    "the frontend {rival:?} holds hosts on the lease server too: the executor runs on exactly one frontend"
                ));
                break;
            }
        }
        executor.step().await;
        tokio::select! {
            _ = stopped.changed() => break,
            () = sleep(executor.poll()) => {}
        }
    }
    executor.shutdown().await;
}
