//! The scheduler's queue and leases, and the time Execute spends before its first
//! operation, on `/metrics` (issue komira-ai/kbf#666): a farm with
//! operations queued and slots idle must be readable from a scrape, not from a trace.

mod support;

use std::time::Duration;

use support::{Cell, Job, done, output, ran};

/// Waits until `read` returns `want`, up to five seconds (the gauges follow the
/// scheduler's tick).
async fn settles(what: &str, read: impl Fn() -> u64, want: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while read() != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what} is {} after 5 s, wanted {want}",
            read()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Catches: an Execute whose time before its first operation, walk, submit and
/// scheduler-lock wait are not observed once each; a directory count that is not the
/// tree's (the job's tree is one directory); a queue gauge that does not rise when an
/// operation waits with no node, nor fall when a lease takes it, a lease gauge that
/// counts the queue or does not return to 0 when the lease ends; and a gauge only
/// refreshed by submit (the lease ends on a result, which is not a submit).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_and_the_leases_are_on_the_scrape() {
    let cell = Cell::start().await;
    let m = cell.cache.metrics();
    let job = Job::new("compile", &[]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    assert_eq!(m.sched_queued.get(), 1, "queued with no node");
    assert_eq!(m.sched_leases.get(), 0);
    assert_eq!(m.execute_presubmit.count(), 1);
    assert_eq!(m.execute_walk.count(), 1);
    assert_eq!(m.execute_submit.count(), 1);
    assert_eq!(m.sched_lock_wait.count(), 1);
    assert_eq!(m.execute_dir_reads.get(), 1);
    assert_eq!(m.execute_presubmit_in_flight.get(), 0);

    let mut node = cell.daemon("node-a", 8, 16).await;
    let start = node.start().await;
    settles("kbf_sched_leases", || m.sched_leases.get(), 1).await;
    settles("kbf_sched_queued", || m.sched_queued.get(), 0).await;

    let compiled = output(&cell, "compiled", 0).await;
    assert!(node.report(ran(start.lease_id, &compiled)).await.accepted);
    done(&mut ops).await;
    settles("kbf_sched_leases", || m.sched_leases.get(), 0).await;
}
