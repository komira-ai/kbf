//! The task that steps an executor beside the listeners: it steps until it is stopped, stops by
//! itself when another frontend holds hosts, and releases what it holds when it ends.

use std::time::Duration;

use kbf_lease_client::{AcquireRequest, Lease, LeaseState, Resources};
use kbf_server::executor::Running;

use crate::rig::{GIB, Rig, count, settle};

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
    let why = running.failed().await;
    assert!(why.contains("front-2"), "{why}");
    running.stop().await;
    assert_eq!(
        count(&rig.requests(), "POST /v1/leases"),
        1,
        "only the rival's"
    );
    assert_eq!(live(&rig).await.len(), 1);
}

/// Catches two frontends that started together, each seeing the other's lease only after
/// its own: the check is made again, and the driver that finds a rival then stops and releases
/// its hosts.
#[tokio::test(start_paused = true)]
async fn a_rival_that_appears_later_stops_the_driver_and_its_hosts_are_released() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let mut running = Running::spawn(rig.restarted_executor());
    pass(&rig, 10).await;
    assert_eq!(live(&rig).await.len(), 1);
    book_for(&rig, "front-2").await;
    let why = {
        let failed = running.failed();
        tokio::pin!(failed);
        loop {
            tokio::select! {
                why = &mut failed => break why,
                () = pass(&rig, 5) => {}
            }
        }
    };
    assert!(why.contains("front-2"), "{why}");
    running.stop().await;
    let held = live(&rig).await;
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0].labels["kbf-frontend"], "front-2");
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
