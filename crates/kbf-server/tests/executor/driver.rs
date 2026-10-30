//! The task that steps an executor beside the listeners: it steps until it is stopped, stops by
//! itself when another frontend holds hosts, and releases what it holds when it ends.

use std::time::Duration;

use kbf_lease_client::{AcquireRequest, Lease, LeaseState, Resources};
use kbf_server::executor::Running;

use crate::rig::{GIB, Rig, count, count_tail, settle};

/// Lets `seconds` pass in steps of 5 s, the farm ticking after each.
async fn pass(rig: &Rig, seconds: u64) {
    for _ in 0..seconds.div_ceil(5) {
        rig.advance(Duration::from_secs(5)).await;
        settle().await;
        rig.farm.tick().await;
        settle().await;
    }
}

async fn live(rig: &Rig) -> Vec<Lease> {
    let leases = rig.api.leases().await.expect("leases");
    leases
        .into_iter()
        .filter(|l| matches!(l.state, LeaseState::Held | LeaseState::Pending))
        .collect()
}

/// Lets time pass in steps of 5 s, at most `steps` of them, until `running` says why it
/// stopped. No wall-clock assertion: a driver that never stops is `None`, not a hang.
async fn stopped_within(rig: &Rig, running: &mut Running, steps: u32) -> Option<String> {
    let failed = running.failed();
    tokio::pin!(failed);
    for _ in 0..steps {
        tokio::select! {
            why = &mut failed => return Some(why),
            () = pass(rig, 5) => {}
        }
    }
    None
}

/// Books a lease for the frontend `frontend`, from the lease server itself.
async fn book_for(rig: &Rig, frontend: &str) {
    let resources = Resources {
        cpu_millis: 2_000,
        memory_bytes: 16 * GIB,
        gpus: 0,
        gpu_class: None,
    };
    let request = AcquireRequest::new("farm-linux", resources, rig.cfg.max_wall)
        .label("kbf-frontend", frontend);
    rig.api.acquire(&request).await.expect("booked");
}

/// Catches a driver that does not step the executor, or leaves its hosts behind: while it
/// runs the executor holds its hosts and the farm has their workers, and once stopped every
/// lease is released and the stop has waited for it.
#[tokio::test(start_paused = true)]
async fn the_driver_holds_hosts_while_it_runs_and_releases_them_when_stopped() {
    let rig = Rig::start(&["node1", "node2"], 2, |_| {}).await;
    let running = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    assert_eq!(live(&rig).await.len(), 2);
    assert!(rig.counted().contains("kbf_executor_leases_held 2\n"));
    running.stop().await;
    assert!(live(&rig).await.is_empty(), "every lease is released");
    assert!(rig.counted().contains("kbf_executor_leases_held 0\n"));
}

/// Catches a driver whose handle is dropped leaving its hosts leased: the task ends with its
/// handle, and releases them as a stop does.
#[tokio::test(start_paused = true)]
async fn a_driver_that_is_dropped_releases_its_hosts() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    let running = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    assert_eq!(live(&rig).await.len(), 1);
    drop(running);
    pass(&rig, 5).await;
    assert!(live(&rig).await.is_empty());
}

/// Catches a second frontend that holds hosts beside this one: the executor runs on exactly one
/// frontend, so the driver that finds another frontend's lease before its first pass says so,
/// and leases nothing.
#[tokio::test(start_paused = true)]
async fn a_rival_found_at_the_start_stops_the_driver_before_it_leases_anything() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    book_for(&rig, "front-2").await;
    let mut running = Running::spawn(rig.restarted_executor());
    let why = stopped_within(&rig, &mut running, 40)
        .await
        .expect("the driver did not stop");
    assert!(why.contains("front-2"), "{why}");
    running.stop().await;
    assert_eq!(
        count(&rig.requests(), "POST /v1/leases"),
        1,
        "only the rival's"
    );
    assert_eq!(live(&rig).await.len(), 1);
}

/// Catches a running driver stopped by a frontend that came after it: the older frontend goes
/// on holding its host through the checks that find the newer one's lease, and the newer one,
/// which stops itself at its own first check, is the one that leaves.
#[tokio::test(start_paused = true)]
async fn a_rival_that_appears_later_does_not_stop_the_older_driver() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let mut running = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    assert_eq!(live(&rig).await.len(), 1);
    book_for(&rig, "front-2").await;
    pass(&rig, 130).await;
    let failed = tokio::time::timeout(Duration::ZERO, running.failed()).await;
    assert!(failed.is_err(), "the older frontend stopped");
    assert_eq!(live(&rig).await.len(), 2, "it holds its host");
    running.stop().await;
    let held = live(&rig).await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].labels["kbf-frontend"], "front-2");
}

/// Catches two frontends that started together, each leasing a host before it saw the other's,
/// both stopping at the next check and the farm left with no executor, or neither stopping and
/// the farm with two: exactly one stops and releases its host, whether the two have different
/// ids or (a config copied to a second frontend) the same one.
#[tokio::test(start_paused = true)]
async fn of_two_frontends_that_started_together_exactly_one_stops() {
    for (first, second) in [("front-1", "front-2"), ("front-1", "front-1")] {
        let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
        let mut a = Running::spawn(rig.other_executor(first));
        let mut b = Running::spawn(rig.other_executor(second));
        pass(&rig, 130).await;
        let stopped = [
            tokio::time::timeout(Duration::ZERO, a.failed())
                .await
                .is_ok(),
            tokio::time::timeout(Duration::ZERO, b.failed())
                .await
                .is_ok(),
        ];
        assert_eq!(
            stopped.iter().filter(|stopped| **stopped).count(),
            1,
            "{first} and {second}: {stopped:?}"
        );
        a.stop().await;
        b.stop().await;
    }
}

/// Catches a second process under the same frontend id that takes the first one's leases and
/// stops their runs (a config copied to a second frontend): it finds the leases renewed while it
/// watches them, stops with an error that says the id is shared, and the first process keeps
/// its host and its run.
#[tokio::test(start_paused = true)]
async fn a_second_process_under_a_live_frontends_id_stops_and_leaves_its_hosts_alone() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let mut first = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    assert_eq!(live(&rig).await.len(), 1);
    let mut second = Running::spawn(rig.restarted_executor());
    let why = stopped_within(&rig, &mut second, 40)
        .await
        .expect("the second process did not stop");
    assert!(why.contains("same id"), "{why}");
    second.stop().await;
    assert_eq!(live(&rig).await.len(), 1, "the first process lost its host");
    assert_eq!(count_tail(&rig.requests(), "DELETE", ""), 0);
    let failed = tokio::time::timeout(Duration::ZERO, first.failed()).await;
    assert!(failed.is_err(), "the first process stopped");
    first.stop().await;
}

/// Catches a list of leases that could not be read being taken for a rival, or for the want of
/// one: nothing is concluded, the executor goes on, and the next check reads it.
#[tokio::test(start_paused = true)]
async fn a_list_that_cannot_be_read_is_no_rival() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.fake
        .inject_on("GET /v1/leases", kbf_lease_client::fake::Fault::Close);
    let mut running = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    let failed = tokio::time::timeout(Duration::ZERO, running.failed()).await;
    assert!(failed.is_err(), "nothing was concluded");
    assert_eq!(live(&rig).await.len(), 1, "and it holds its host");
    running.stop().await;
}

/// Catches leases that are not another frontend's being taken for one: a lease of another
/// frontend that has been released, a lease with no frontend label at all, and the frontend's
/// own are no rival, and the driver goes on and leases its host.
#[tokio::test(start_paused = true)]
async fn leases_that_are_not_a_live_lease_of_another_frontend_are_no_rival() {
    let rig = Rig::start(&["node1", "node2", "node3"], 1, |_| {}).await;
    let resources = Resources {
        cpu_millis: 2_000,
        memory_bytes: 16 * GIB,
        gpus: 0,
        gpu_class: None,
    };
    let unlabelled = AcquireRequest::new("farm-linux", resources, rig.cfg.max_wall);
    rig.api.acquire(&unlabelled).await.expect("booked");
    book_for(&rig, "front-2").await;
    let released = rig
        .api
        .leases()
        .await
        .expect("leases")
        .into_iter()
        .find(|l| l.labels.get("kbf-frontend").is_some_and(|v| v == "front-2"))
        .expect("the booking");
    rig.api.release(released.id).await.expect("released");

    let mut running = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    let failed = tokio::time::timeout(Duration::ZERO, running.failed()).await;
    assert!(failed.is_err(), "no rival");
    let ours = live(&rig)
        .await
        .into_iter()
        .filter(|l| l.labels.get("kbf-frontend").is_some_and(|v| v == "front-1"))
        .count();
    assert_eq!(ours, 1);
    running.stop().await;
}
