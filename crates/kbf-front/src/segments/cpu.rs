//! The bound on segment hashing: CPU work for record digests runs on the blocking pool,
//! never on a runtime worker, and at most `--cpu-threads` of it at once, so a burst of
//! ranges cannot take every core from the rest of the server.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tokio::sync::Semaphore;

/// At most `threads` hashing jobs running at once.
#[derive(Debug)]
pub(super) struct HashPool {
    permits: Arc<Semaphore>,
}

impl HashPool {
    pub(super) fn new(threads: NonZeroUsize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(threads.get())),
        }
    }

    /// Runs `job` on the blocking pool once a permit is free; the permit is held for
    /// the job's whole run. The waiting and the job live in a task of their own, so a
    /// caller cancelled meanwhile (even before it had a permit) still sees the job run.
    pub(super) async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, tokio::task::JoinError> {
        let permits = Arc::clone(&self.permits);
        tokio::spawn(async move {
            // The semaphore is never closed.
            let Ok(permit) = permits.acquire_owned().await else {
                unreachable!("the hash pool's semaphore is never closed");
            };
            tokio::task::spawn_blocking(move || {
                let out = job();
                drop(permit);
                out
            })
            .await
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn two() -> NonZeroUsize {
        NonZeroUsize::MIN.saturating_add(1)
    }

    /// Eight jobs offered at once to a pool of two run two at a time: never more (the
    /// bound), and two together (it is a pool, not a lock).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn at_most_the_configured_jobs_run_together() {
        let pool = Arc::new(HashPool::new(two()));
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (pool, now, peak) = (Arc::clone(&pool), Arc::clone(&now), Arc::clone(&peak));
            tasks.push(tokio::spawn(async move {
                pool.run(move || {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    now.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    /// A job runs on a thread of its own, not on the thread that awaits it (here the
    /// runtime's only thread, standing for a worker).
    #[tokio::test]
    async fn jobs_run_off_the_awaiting_thread() {
        let here = std::thread::current().id();
        let there = HashPool::new(NonZeroUsize::MIN)
            .run(|| std::thread::current().id())
            .await
            .unwrap();
        assert_ne!(here, there);
    }

    /// A caller that gives up while its job still waits for a permit does not cancel
    /// the job: the segment's records stay claimed until the job records them.
    #[tokio::test]
    async fn a_cancelled_caller_does_not_cancel_its_job() {
        let pool = Arc::new(HashPool::new(NonZeroUsize::MIN));
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let first = {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move {
                pool.run(move || {
                    let _ = hold_rx.recv();
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        let ran = Arc::new(AtomicUsize::new(0));
        let second = {
            let (pool, ran) = (Arc::clone(&pool), Arc::clone(&ran));
            tokio::spawn(async move {
                pool.run(move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                })
                .await
            })
        };
        tokio::task::yield_now().await;
        second.abort();
        hold_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        for _ in 0..200 {
            if ran.load(Ordering::SeqCst) == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the abandoned job never ran");
    }
}
