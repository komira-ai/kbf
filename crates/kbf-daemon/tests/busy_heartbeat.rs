//! The busy rate on a daemon's heartbeats: a daemon given the actions cgroup's `cpu.stat`
//! (`Daemon::with_cpu_meter`) sends `busy_millis` from the second heartbeat on, and a
//! daemon given none sends none.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::FakeRuntime;
use support::{Harness, scratch};

fn write(path: &std::path::Path, usage: u64) {
    std::fs::write(path, format!("usage_usec {usage}\nuser_usec 0\n")).expect("write cpu.stat");
}

/// Asserts that the first heartbeat has no rate (it has no window), that the next one
/// after the counter grew carries a positive rate, and that the one after that, with the
/// counter unchanged, carries a rate of exactly 0 (idle is a reading, not an absence).
/// Catches a rate sent on the first heartbeat, a rate that never reaches the wire, and an
/// unchanged counter sent as no reading. The rate's magnitude is not asserted: the
/// window is wall time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_with_a_meter_sends_the_busy_rate_from_its_second_heartbeat() {
    let stat = scratch("busy-heartbeat").join("cpu.stat");
    write(&stat, 5_000_000);
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let mut h = Harness::with_cpu_meter("busy-heartbeat", runtime, stat.clone()).await;
    let mut peer = h.welcomed().await;

    let first = peer.heartbeat().await;
    assert_eq!(first.busy_millis, None);

    // A million CPU-seconds in the window: positive however long the window was.
    write(&stat, 1_005_000_000_000);
    let grew = peer.heartbeat().await;
    assert!(
        grew.busy_millis.is_some_and(|busy| busy > 0),
        "{:?}",
        grew.busy_millis
    );

    let idle = peer.heartbeat().await;
    assert_eq!(idle.busy_millis, Some(0));
}

/// Asserts that a daemon given no meter, as one on a node without an actions cgroup is,
/// sends no rate in any heartbeat. Catches a default of 0 sent for "unmeasured", which the
/// server would read as an idle node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_a_meter_sends_no_rate() {
    let off = Duration::from_secs(60);
    let mut h = Harness::start("busy-heartbeat-off", Duration::ZERO, off).await;
    let mut peer = h.welcomed().await;
    let first = peer.heartbeat().await;
    let next = peer.heartbeat().await;
    assert_eq!((first.busy_millis, next.busy_millis), (None, None));
}
