//! [`CpuPool`]: a fixed number of threads for work that keeps a core busy (hashing an
//! action's output files), apart from tokio's runtime and blocking pool.
//!
//! The blocking pool grows to hundreds of threads for system calls that wait; hashing on
//! it would put as many hashing threads on the node's cores as there are files in flight.
//! This pool runs at most `--cpu-threads` jobs at once, however many leases ask, and
//! queues the rest in the order they came. A job that panics is reported to its caller
//! and does not take its thread down.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

/// `--cpu-threads` by default.
pub const DEFAULT_CPU_THREADS: usize = 4;

type Job = Box<dyn FnOnce() + Send>;

/// Threads that run closures one each, in the order they were submitted. Cloning shares
/// the threads; they end when the last clone is dropped, after the jobs queued.
#[derive(Clone, Debug)]
pub struct CpuPool {
    threads: usize,
    sender: Sender<Job>,
}

impl CpuPool {
    /// A pool of `threads` (at least 1) threads named `kbf-cpu-<n>`.
    ///
    /// # Panics
    /// The operating system refused to start a thread.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        let threads = threads.max(1);
        let (sender, receiver) = channel::<Job>();
        let receiver = Arc::new(Mutex::new(receiver));
        for n in 0..threads {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("kbf-cpu-{n}"))
                .spawn(move || work(&receiver))
                .expect("start a cpu pool thread");
        }
        Self { threads, sender }
    }

    /// How many jobs run at once.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Runs `job` on a pool thread when one is free, and returns its result.
    ///
    /// # Errors
    /// The job panicked (or never ran: the pool's threads outlive every handle on it, so
    /// only a panic ends a job early).
    pub async fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        let (done, result) = oneshot::channel();
        let job: Job = Box::new(move || {
            // The caller may have gone (its deadline passed): the result is not wanted.
            let _ = done.send(job());
        });
        // The threads end only when every sender is gone, and `self` is one: a send that
        // fails drops the job, and with it `done`, which `result` reports below.
        let _ = self.sender.send(job);
        result
            .await
            .map_err(|_| io::Error::other("a cpu pool job panicked or did not run"))
    }
}

fn work(receiver: &Mutex<Receiver<Job>>) {
    loop {
        // The lock is held while waiting for a job and released before running it, so
        // one idle thread waits on the queue and the others on the lock.
        let job = receiver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv();
        let Ok(job) = job else { return };
        // A panic drops the job's sender, which its caller reports.
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
}

#[cfg(test)]
#[path = "cpu_pool_tests.rs"]
mod tests;
