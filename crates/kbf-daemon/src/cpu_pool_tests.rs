//! The pool's size, order and survival of a panicking job.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use super::*;

/// Catches a pool that runs fewer or more jobs at once than it was given threads for
/// (mutant: `threads.max(1)` replaced by a constant 1, or by twice the count): with 3
/// threads, 3 jobs that wait for each other at a barrier all finish (so 3 run at once),
/// and the most ever running while 12 jobs are queued is 3.
#[tokio::test]
async fn it_runs_as_many_jobs_at_once_as_it_has_threads() {
    let pool = CpuPool::new(3);
    assert_eq!(pool.threads(), 3);
    let barrier = Arc::new(Barrier::new(3));
    let jobs: Vec<_> = (0..3)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            pool.run(move || {
                barrier.wait();
            })
        })
        .collect();
    for done in futures::future::join_all(jobs).await {
        done.expect("ran");
    }
    let (running, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let jobs: Vec<_> = (0..12)
        .map(|_| {
            let (running, most) = (Arc::clone(&running), Arc::clone(&most));
            pool.run(move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(20));
                running.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();
    for done in futures::future::join_all(jobs).await {
        done.expect("ran");
    }
    assert_eq!(most.load(Ordering::SeqCst), 3);
}

/// Catches a pool of no threads, which would never run a job, and a panic that kills a
/// thread or goes unreported (mutants: the `max(1)` dropped; the `catch_unwind` dropped,
/// which leaves the single thread dead so the next job never runs): a job that panics
/// is an error to its caller, and the one thread runs the next job.
#[tokio::test]
async fn a_job_that_panics_is_an_error_and_the_thread_goes_on() {
    let pool = CpuPool::new(0);
    assert_eq!(pool.threads(), 1);
    let failed = pool.run(|| -> u8 { panic!("a job's own bug") }).await;
    assert!(failed.is_err());
    assert_eq!(pool.run(|| 7).await.expect("ran"), 7);
}

/// Catches jobs not run in the order they were submitted: on one thread, ten jobs
/// record their number in submission order.
#[tokio::test]
async fn jobs_run_in_the_order_they_were_submitted() {
    let pool = CpuPool::new(1);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let jobs: Vec<_> = (0..10)
        .map(|n| {
            let seen = Arc::clone(&seen);
            pool.run(move || seen.lock().expect("seen").push(n))
        })
        .collect();
    for done in futures::future::join_all(jobs).await {
        done.expect("ran");
    }
    assert_eq!(*seen.lock().expect("seen"), (0..10).collect::<Vec<_>>());
}
