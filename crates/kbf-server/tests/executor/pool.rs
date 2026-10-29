//! The pool of leases: acquired, kept, rotated, found again after a restart, and released.

use std::time::Duration;

use kbf_lease_client::LeaseState;
use kbf_server::executor::{Clock, Config, ConfigError, Deps, Executor};
use kbf_types::LeaseKind;

use crate::rig::{Rig, count, count_tail};

fn short_leases(cfg: &mut Config) {
    cfg.max_run = Duration::from_secs(30);
    cfg.default_timeout = Duration::from_secs(30);
    cfg.run_margin = Duration::from_secs(30);
    cfg.lease_ttl = Duration::from_secs(60);
    cfg.drain_before_end = Duration::from_secs(120);
    cfg.max_wall = Duration::from_secs(300);
}

/// Catches an executor that reaches for the lease server before it is run: building one asks
/// for nothing, and a config that breaks a rule is refused before anything is built. (What a
/// server without an executor never does is for the wiring to test: nothing builds one yet.)
#[tokio::test(start_paused = true)]
async fn building_an_executor_asks_the_lease_server_for_nothing() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    assert!(rig.requests().is_empty());
    assert!(rig.exec.hosts().is_empty());
    let deps = Deps {
        farm: rig.farm.clone(),
        client: rig.fake.client().expect("a client"),
        ingest: rig.ingest.clone(),
        presign: rig.presign.clone(),
        inputs: rig.inputs.clone(),
    };
    let none = Config {
        hosts: 0,
        ..rig.cfg.clone()
    };
    let refused = Executor::new(none, deps, Clock::from_unix_ms(0));
    assert_eq!(refused.err(), Some(ConfigError::NoHosts));
    assert!(rig.requests().is_empty());
}

/// Catches a pool that leases one host twice or none at all: each lease is a host of its own,
/// labelled with the frontend, and each host is a worker of its own whose only driver is
/// `container`: a whole-machine action is never placed on one.
#[tokio::test(start_paused = true)]
async fn each_lease_is_a_host_and_a_worker_that_takes_only_containers() {
    let mut rig = Rig::start(&["node1", "node2", "node3"], 2, |_| {}).await;
    rig.pump().await;
    let hosts = rig.exec.hosts();
    let workers: Vec<String> = hosts.iter().map(|h| h.worker.to_string()).collect();
    assert_eq!(workers, ["js:node1", "js:node2"]);
    let leases = rig.api.leases().await.expect("leases");
    assert_eq!(leases.len(), 2);
    assert!(
        leases
            .iter()
            .all(|l| l.labels.get("kbf-frontend").map(String::as_str) == Some("front-1"))
    );
    let whole = rig.submit_kind(1, LeaseKind::WholeMachine);
    rig.pump().await;
    assert!(
        rig.runs().await.is_empty(),
        "a whole-machine lease was sent to a host"
    );
    assert!(whole.finished().is_none());
    let action = rig.submit(2);
    rig.pump().await;
    assert_eq!(rig.runs_of(2).await.len(), 1);
    drop(action);
}

/// Catches a lease that is `202 pending` (its host is being prepared) being used before it
/// is held, or never used after: the slot waits for it, then registers the worker.
#[tokio::test(start_paused = true)]
async fn a_booked_lease_waits_until_its_host_is_ready() {
    let mut rig = Rig::built(&["node1"], 1, 16, true, |_| {}).await;
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty());
    let lease = rig.api.leases().await.expect("leases")[0].id;
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty(), "used before it was held");
    assert_eq!(
        count(&rig.requests(), "POST /v1/leases"),
        1,
        "asked for another"
    );
    assert!(rig.fake.ready(lease));
    rig.pump().await;
    assert_eq!(rig.exec.hosts()[0].lease, lease);
}

/// Catches a booked lease that vanishes from the lease server (its prepare timed out)
/// leaving the slot waiting for it for ever: the slot is asked for again.
#[tokio::test(start_paused = true)]
async fn a_booked_lease_that_vanishes_is_asked_for_again() {
    let mut rig = Rig::built(&["node1"], 1, 16, true, |_| {}).await;
    rig.pump().await;
    let lease = rig.api.leases().await.expect("leases")[0].id;
    rig.api.release(lease).await.expect("released");
    rig.pump().await;
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 2);
}

/// Catches a restarted frontend that asks for new hosts while its old leases run on (the pool
/// would double), or leaves the runs of the frontend that died where they are (nothing of the
/// farm maps to them any more, and each holds its host's budget): it finds its leases by
/// label, stops their runs, and holds the same hosts; the farm, which still has the lease
/// the dead frontend's worker held, gives it up after the grace and the action runs again.
#[tokio::test(start_paused = true)]
async fn a_restarted_frontend_adopts_its_leases_and_stops_their_runs() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    let lease = rig.exec.hosts()[0].lease;
    rig.exec = rig.restarted_executor();
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    assert_eq!(
        rig.exec.hosts()[0].lease,
        lease,
        "a new lease was asked for"
    );
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
    let stopped = format!("DELETE /v1/leases/{lease}/runs/{}", first.run);
    assert_eq!(count(&rig.requests(), &stopped), 1);
    rig.elapse(70).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(
        runs.len(),
        2,
        "the farm did not run the action again: {runs:?}"
    );
    rig.host_finishes(&runs[1].run, kbf_lease_client::Outcome::Succeeded, Some(0));
    assert!(matches!(
        rig.until_done(&caller, 4).await,
        kbf_front::Finished::Ran(_)
    ));
}

/// Catches an adopted pool larger than the one asked for, and a lease still being booked when
/// the frontend restarts being forgotten: a lease beyond the pool's size is released, a
/// booked one is waited for as if it were this run's own.
#[tokio::test(start_paused = true)]
async fn adoption_releases_what_the_pool_does_not_need_and_waits_for_what_is_booked() {
    let mut rig = Rig::start(&["node1", "node2"], 2, |_| {}).await;
    rig.pump().await;
    let leases: Vec<_> = rig.exec.hosts().iter().map(|h| h.lease).collect();
    assert_eq!(leases.len(), 2);
    rig.cfg.hosts = 1;
    rig.exec = rig.restarted_executor();
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{}", leases[1])),
        1
    );
    assert_eq!(rig.api.leases().await.expect("leases").len(), 1);

    let mut booked = Rig::built(&["node1"], 1, 16, true, |_| {}).await;
    booked.pump().await;
    let lease = booked.api.leases().await.expect("leases")[0].id;
    booked.exec = booked.restarted_executor();
    booked.pump().await;
    assert_eq!(count(&booked.requests(), "POST /v1/leases"), 1);
    assert!(booked.fake.ready(lease));
    booked.pump().await;
    assert_eq!(booked.exec.hosts()[0].lease, lease);
    let state = booked.api.leases().await.expect("leases")[0].state;
    assert_eq!(state, LeaseState::Held);
}

/// Catches a lease that expires under a busy pool: it is renewed every third of its ttl for
/// as long as the executor runs, and the lease server never lets go of it.
#[tokio::test(start_paused = true)]
async fn the_lease_is_renewed_for_as_long_as_the_executor_runs() {
    let mut rig = Rig::start(&["node1"], 1, short_leases).await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    rig.elapse(150).await;
    assert_eq!(rig.api.leases().await.expect("leases")[0].id, lease);
    let renewals = count(&rig.requests(), &format!("POST /v1/leases/{lease}/renew"));
    assert!(renewals >= 5, "{renewals} renewals in 150 s of a 60 s ttl");
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
}

/// Catches a lease run to the end of its wall time under a worker that still takes work, or
/// cut off under a run that has not ended: a worker whose lease nears its end stops taking
/// work (the farm sees it draining), is released once its run ends, and the slot is filled
/// by a new lease and a new worker.
#[tokio::test(start_paused = true)]
async fn a_lease_near_its_end_is_drained_released_and_replaced() {
    let mut rig = Rig::start(&["node1"], 1, short_leases).await;
    rig.pump().await;
    let first = rig.exec.hosts()[0].lease;
    rig.inputs
        .facts
        .lock()
        .expect("facts")
        .as_mut()
        .expect("ok")
        .timeout = Some(Duration::from_secs(20));
    let caller = rig.submit(1);
    rig.pump().await;
    let run = rig.runs_of(1).await.remove(0);
    // 100 s in: not yet time to drain (the lease ends at 300 s, the drain starts 120 s before).
    rig.elapse(100).await;
    assert!(!rig.exec.hosts()[0].draining);
    rig.elapse(100).await;
    let host = rig.exec.hosts()[0].clone();
    assert!(host.draining, "still taking work 100 s from the end");
    assert_eq!(host.runs, 1);
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{first}")),
        0,
        "released with a run live"
    );
    // An action the farm has not placed is not placed on a draining worker.
    let waiting = rig.submit(2);
    rig.pump().await;
    assert!(rig.runs_of(2).await.is_empty());
    rig.host_finishes(&run.run, kbf_lease_client::Outcome::Succeeded, Some(0));
    assert!(matches!(
        rig.until_done(&caller, 6).await,
        kbf_front::Finished::Ran(_)
    ));
    rig.pump().await;
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{first}")),
        1
    );
    // The host is free once the run the release stopped has ended.
    rig.host_ends(&run.run);
    rig.elapse(10).await;
    let replacement = rig.exec.hosts()[0].lease;
    assert_ne!(replacement, first);
    assert!(!rig.exec.hosts()[0].draining);
    // The new worker takes the action that was waiting.
    rig.elapse(5).await;
    assert_eq!(rig.runs_of(2).await.len(), 1);
    drop(waiting);
}

/// Catches a tainted host that goes on taking work: the lease server's host list says so, the
/// worker drains, and the lease is released.
#[tokio::test(start_paused = true)]
async fn a_tainted_host_is_drained_and_released() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    assert!(rig.fake.taint_host("node1", "post-job-check"));
    rig.advance(Duration::from_secs(6)).await;
    rig.pump().await;
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{lease}")),
        1
    );
}

/// Catches a slot that asks again at every step (a request storm on a full pool), or never:
/// a lease refused for want of a host is asked for again after the retry delay, and a pool of
/// fewer hosts than slots holds what it can.
#[tokio::test(start_paused = true)]
async fn a_slot_without_a_host_asks_again_after_the_retry_delay() {
    let mut rig = Rig::start(&["node1"], 2, |_| {}).await;
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 2);
    rig.pump().await;
    assert_eq!(
        count(&rig.requests(), "POST /v1/leases"),
        2,
        "asked again at once"
    );
    rig.advance(Duration::from_secs(5)).await;
    rig.pump().await;
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 3);
}

/// Catches a shutdown that leaves runs or leases behind: every run is asked to stop, every
/// lease is released, and the executor holds nothing.
#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_runs_and_releases_the_leases() {
    let mut rig = Rig::start(&["node1", "node2"], 2, |_| {}).await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let run = rig.runs_of(1).await.remove(0);
    let leases: Vec<_> = rig.exec.hosts().iter().map(|h| h.lease).collect();
    rig.exec.shutdown().await;
    assert!(rig.exec.hosts().is_empty());
    assert!(rig.api.leases().await.expect("leases").is_empty());
    let requests = rig.requests();
    assert_eq!(
        count_tail(&requests, "DELETE", &format!("/runs/{}", run.run)),
        1
    );
    for lease in leases {
        assert_eq!(count(&requests, &format!("DELETE /v1/leases/{lease}")), 1);
    }
}
