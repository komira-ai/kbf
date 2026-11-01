//! Work that waits on the kernel, off the async workers.
//!
//! A daemon has a few async workers (`--async-threads`), and everything runs on them:
//! the stream to the server and its heartbeat, every lease's task. A task that reads a
//! file or waits for a child process without yielding holds its worker until the kernel
//! answers, and a cgroup file, a journal commit or a page fault can take seconds. So
//! such work runs on tokio's blocking pool, which has threads to spare for it
//! (`--blocking-threads`); hashing, the digests of an action's output files and of its
//! Directory messages, runs on the CPU pool (`kbf_daemon::cpu_pool`), which is bounded
//! by the node's cores.

/// Runs `work` on the blocking pool and returns its result. A panic in `work` panics the
/// caller.
///
/// Dropping the returned future does not stop `work`: it runs to its end and its result is
/// dropped. Callers keep to work that is safe to finish unwatched.
pub(crate) async fn off_worker<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(done) => done,
        Err(gone) => std::panic::resume_unwind(gone.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches work run on the caller's own thread, an async worker, and a result that is
    /// lost on the way back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_work_runs_on_the_blocking_pool_and_its_result_comes_back() {
        let caller = std::thread::current().id();
        let worker = off_worker(|| std::thread::current().id()).await;
        assert_ne!(worker, caller);
        assert_eq!(off_worker(|| 7).await, 7);
    }

    /// Catches a panic in the work being swallowed into a default result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[should_panic(expected = "the work failed")]
    async fn a_panic_in_the_work_is_the_callers() {
        off_worker(|| panic!("the work failed")).await;
    }
}
