//! A host whose scratch disk quota is out of order refuses every start `quota-unavailable`.
//! It is the host's fault and not a taint: the farm runs the action again elsewhere at no
//! cost to the caller, the host is kept out of use for a while, and it is used again after.

use std::time::Duration;

use kbf_front::Finished;
use kbf_lease_client::Outcome;

use crate::rig::{Rig, count, count_tail};

const QUOTA: &str = "quota-unavailable";

fn hosts_held(rig: &Rig) -> Vec<String> {
    rig.exec.hosts().into_iter().map(|h| h.host).collect()
}

fn ended(rig: &Rig, outcome: &str, n: u32) -> bool {
    rig.counted().contains(&format!(
        "kbf_executor_runs_total{{outcome=\"{outcome}\"}} {n}\n"
    ))
}

fn ended_any(rig: &Rig, outcome: &str) -> bool {
    let row = format!("kbf_executor_runs_total{{outcome=\"{outcome}\"}} ");
    rig.counted().lines().any(|l| {
        l.strip_prefix(&row)
            .is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n > 0))
    })
}

/// Catches a start the host refused for its quota being answered as the action's
/// infrastructure failure: it is lost to the host, so the action runs again on another host
/// and the caller sees only that run's result. The run ends as `lost` and never as `infra`.
#[tokio::test(start_paused = true)]
async fn a_start_refused_for_the_quota_is_lost_and_rerun_on_another_host() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let sick = rig.exec.hosts().remove(0);
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    assert_eq!(first.lease, sick.lease);
    rig.host_finishes_with(&first.run, Outcome::FailedToStart, None, QUOTA);
    rig.pump().await;
    assert!(caller.finished().is_none(), "answered at the refusal");
    assert!(ended(&rig, "lost", 1), "{}", rig.counted());
    assert!(ended(&rig, "infra", 0), "{}", rig.counted());
    rig.elapse(15).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 2, "not rerun: {runs:?}");
    assert_ne!(runs[1].host, sick.host, "rerun on the same host");
    rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a host that keeps taking runs after it refused one for its quota, and a host
/// that is never used again: it gets no run while the hold lasts (its lease is held idle, so
/// the lease server cannot hand it out again and the farm sees no worker on it), and once the
/// hold has passed the lease is released and the host is taken and used again.
#[tokio::test(start_paused = true)]
async fn the_host_is_held_out_for_the_hold_and_used_again_after_it() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| {
        cfg.quota_hold = Duration::from_secs(120);
    })
    .await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    rig.host_finishes_with(&first.run, Outcome::FailedToStart, None, QUOTA);
    rig.elapse(60).await;
    // The host stops listing the refused run, as it does, so the lease can end.
    rig.host_ends(&first.run);
    let delete = format!("DELETE /v1/leases/{lease}");
    assert!(hosts_held(&rig).is_empty(), "{:?}", rig.exec.hosts());
    assert_eq!(count(&rig.requests(), &delete), 0, "handed back too early");
    assert_eq!(count_tail(&rig.requests(), "POST", "/runs"), 1);
    assert!(caller.finished().is_none());
    rig.elapse(100).await;
    assert_eq!(count(&rig.requests(), &delete), 1, "never handed back");
    let back = rig.exec.hosts();
    assert_eq!(hosts_held(&rig), ["node1"], "{back:?}");
    assert!(!back[0].draining);
    let runs = rig.runs_of(1).await;
    // The refused run is not listed any more: the one listed is the rerun, on a new lease
    // of the recovered host.
    assert_eq!(runs.len(), 1, "not rerun on the recovered host: {runs:?}");
    assert_eq!(runs[0].host, "node1");
    assert_ne!(runs[0].lease, lease);
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches the refusal at queue time (the same code as the lease server's error) being
/// treated as an infrastructure failure, or the worker going on: the action is lost to the
/// host and rerun on another one, and the sick host stays leased but idle.
#[tokio::test(start_paused = true)]
async fn a_post_refused_for_the_quota_is_lost_and_the_host_is_held_out() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let sick = rig.exec.hosts().remove(0);
    assert!(rig.fake.refuse_posts(&sick.host, Some(QUOTA)));
    let caller = rig.submit(1);
    rig.pump().await;
    assert!(caller.finished().is_none());
    let posted = rig.runs().await;
    assert!(posted.iter().all(|p| p.host != sick.host), "{posted:?}");
    assert!(ended(&rig, "lost", 1), "{}", rig.counted());
    assert!(ended(&rig, "infra", 0), "{}", rig.counted());
    rig.elapse(15).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_ne!(runs[0].host, sick.host);
    let delete = format!("DELETE /v1/leases/{}", sick.lease);
    assert_eq!(count(&rig.requests(), &delete), 0);
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a taint being handled like a sick quota (or the other way round): the tainted
/// host is an infrastructure failure of the action, drained, and its lease is handed back
/// at once; nothing is held out, and nothing is lost to the host.
#[tokio::test(start_paused = true)]
async fn a_taint_is_still_infrastructure_and_the_lease_is_handed_back_at_once() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let sick = rig.exec.hosts().remove(0);
    assert!(rig.fake.taint_host(&sick.host, "post-job-check"));
    let _caller = rig.submit(1);
    rig.elapse(15).await;
    assert!(ended_any(&rig, "infra"), "{}", rig.counted());
    assert!(!ended_any(&rig, "lost"), "{}", rig.counted());
    let delete = format!("DELETE /v1/leases/{}", sick.lease);
    assert_eq!(count(&rig.requests(), &delete), 1);
}

/// Catches a refusal the executor does not know being taken for the quota's: it is an
/// infrastructure failure of the action, and the host is neither drained nor held out.
#[tokio::test(start_paused = true)]
async fn an_unknown_refusal_is_still_infrastructure_and_the_host_is_kept() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    assert!(rig.fake.refuse_posts("node1", Some("quota-whatever")));
    let _caller = rig.submit(1);
    rig.pump().await;
    assert!(ended_any(&rig, "infra"), "{}", rig.counted());
    assert!(!ended_any(&rig, "lost"), "{}", rig.counted());
    let held = rig.exec.hosts();
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(!held[0].draining);
    let delete = format!("DELETE /v1/leases/{lease}");
    assert_eq!(count(&rig.requests(), &delete), 0);

    // The same words as the detail of a start that failed are infrastructure too.
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    rig.host_finishes_with(&first.run, Outcome::FailedToStart, None, "scratch-failed");
    rig.pump().await;
    assert!(ended(&rig, "infra", 1), "{}", rig.counted());
    assert!(!rig.exec.hosts()[0].draining);
}

/// Catches a shutdown that leaves the lease of a held-out host booked (it would keep the
/// host busy until its ttl ends): it is released with the rest.
#[tokio::test(start_paused = true)]
async fn a_shutdown_releases_the_lease_of_a_host_held_out() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    let _caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    rig.host_finishes_with(&first.run, Outcome::FailedToStart, None, QUOTA);
    rig.elapse(15).await;
    let delete = format!("DELETE /v1/leases/{lease}");
    assert_eq!(count(&rig.requests(), &delete), 0);
    rig.exec.shutdown().await;
    assert_eq!(count(&rig.requests(), &delete), 1);
}
