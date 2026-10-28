//! Admission by measured CPU, end to end in one process: a daemon's heartbeat carries
//! `busy_millis` over the worker stream and the server's scheduler places by it
//! (`--admit-busy-percent`). A module of the `execute` test binary.

use std::time::Duration;

use kbf_sched::CpuAdmission;

use crate::support::{Cell, Job, done, ran, response};

/// Asserts that a reading on a heartbeat decides placement: a node of 4 CPUs whose last
/// heartbeat said 4000 CPU-milliseconds per second (all of them) is given no second
/// action, the second stays queued in the farm, and the next heartbeat saying 0 gets it
/// placed and started. Catches a daemon's reading dropped between the wire and the
/// scheduler (the second action would start at once), one read from the wrong heartbeat
/// field, and a reading that is stored but does not place the waiting action.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heartbeat_reading_holds_back_and_then_releases_an_action() {
    let admission = CpuAdmission::new(50, Duration::from_secs(5));
    let cell = Cell::start_with_cpu_admission(admission).await;
    let mut daemon = cell.daemon("busy-node", 4, 64).await;
    let (first, second) = (Job::new("first", &[]), Job::new("second", &[]));
    cell.upload(&first.blobs()).await;
    cell.upload(&second.blobs()).await;

    let mut first_ops = cell.execute(&first.action).await;
    let start = daemon.start().await;
    let running = [start.lease_id.expect("a lease id")];
    daemon.beat_busy_acked(&running, 4_000).await;

    let mut second_ops = cell.execute(&second.action).await;
    assert_eq!(
        cell.queued_operations(),
        1,
        "the full node takes no second action"
    );

    daemon.beat_busy_acked(&running, 0).await;
    let later = daemon.start().await;
    assert_eq!(cell.queued_operations(), 0);

    let result = crate::support::output(&cell, "first", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut first_ops).await).result, Some(result));
    let result = crate::support::output(&cell, "second", 0).await;
    assert!(daemon.report(ran(later.lease_id, &result)).await.accepted);
    assert_eq!(response(&done(&mut second_ops).await).result, Some(result));
}
