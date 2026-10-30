//! The pool of leases: acquired, kept, rotated, found again after a restart, and released.

use std::sync::Arc;
use std::time::Duration;

use kbf_lease_client::LeaseState;
use kbf_lease_client::fake::Fault;
use kbf_server::executor::{Clock, Config, ConfigError, Deps, Executor};
use kbf_types::LeaseKind;

use crate::rig::{Rig, count, count_tail};

fn short_leases(cfg: &mut Config) {
    cfg.max_run = Duration::from_secs(30);
    cfg.default_timeout = Duration::from_secs(30);
    cfg.run_margin = Duration::from_secs(20);
    cfg.lease_ttl = Duration::from_secs(60);
    cfg.drain_before_end = Duration::from_secs(120);
    cfg.max_wall = Duration::from_secs(300);
}

/// Catches an executor that reaches for the lease server before it is run: building one asks
/// for nothing, and a config that breaks a rule is refused before anything is built. (What a
/// server without an executor never does is tested at the frontend, in src/frontend/executor_tests.rs.)
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
        metrics: Arc::default(),
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

/// Catches a booked lease being given up because the list of leases could not be read: a
/// transport error is not an empty list, and a second lease booked beside the first would hold
/// a second host, the first never renewed or released. The slot waits, and the lease is
/// used once its host is ready.
#[tokio::test(start_paused = true)]
async fn a_booked_lease_survives_a_list_that_could_not_be_read() {
    let mut rig = Rig::built(&["node1", "node2"], 1, 16, true, |_| {}).await;
    rig.exec.step().await;
    let lease = rig.api.leases().await.expect("leases")[0].id;
    for fault in [
        Fault::Close,
        Fault::Reply {
            status: 503,
            body: bytes::Bytes::new(),
        },
    ] {
        rig.fake.inject_on("GET /v1/leases", fault);
        rig.advance(Duration::from_secs(1)).await;
        rig.exec.step().await;
    }
    assert_eq!(
        count(&rig.requests(), "POST /v1/leases"),
        1,
        "a second lease was booked"
    );
    assert!(rig.fake.ready(lease));
    rig.pump().await;
    assert_eq!(rig.exec.hosts()[0].lease, lease);
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
}

/// Catches the check for strays releasing a lease that is booked but not ready: a slot waiting
/// for its host holds that lease, and releasing it would book another each time.
#[tokio::test(start_paused = true)]
async fn the_check_for_strays_does_not_release_a_booked_lease() {
    let mut rig = Rig::built(&["node1", "node2"], 1, 16, true, |cfg| {
        cfg.reconcile_every = Duration::from_secs(1);
    })
    .await;
    rig.exec.step().await;
    let lease = rig.api.leases().await.expect("leases")[0].id;
    for _ in 0..3 {
        rig.advance(Duration::from_secs(2)).await;
        rig.exec.step().await;
    }
    assert_eq!(state_of(&rig, lease).await, Some(LeaseState::Pending));
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{lease}")),
        0
    );
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
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
    rig.exec.step().await;
    rig.watch_over().await;
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
    rig.exec.step().await;
    rig.watch_over().await;
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
    booked.exec.step().await;
    booked.watch_over().await;
    booked.pump().await;
    assert_eq!(count(&booked.requests(), "POST /v1/leases"), 1);
    assert!(booked.fake.ready(lease));
    booked.pump().await;
    assert_eq!(booked.exec.hosts()[0].lease, lease);
    let state = booked.api.leases().await.expect("leases")[0].state;
    assert_eq!(state, LeaseState::Held);
}

/// Catches a restarted frontend that takes the leases of an earlier process at once: that
/// process may be alive (a config copied to a second frontend), and its runs would be stopped.
/// Nothing is taken, stopped or asked for while the leases are watched for a renewal; once they
/// are seen not to be renewed, they are adopted.
#[tokio::test(start_paused = true)]
async fn an_earlier_process_leases_are_taken_only_after_they_are_seen_not_to_be_renewed() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    let first = rig.runs_of(1).await.remove(0);
    rig.exec = rig.restarted_executor();
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty(), "taken at once");
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
    assert_eq!(count_tail(&rig.requests(), "DELETE", ""), 0);
    rig.watch_over().await;
    rig.pump().await;
    assert_eq!(rig.exec.hosts()[0].lease, lease);
    let stopped = format!("DELETE /v1/leases/{lease}/runs/{}", first.run);
    assert_eq!(count(&rig.requests(), &stopped), 1, "its run");
    assert!(rig.exec.twin().is_none());
}

/// Catches a second process under a live executor's frontend id adopting its lease: stepping
/// both, the lease is renewed while the second watches, so it concludes the id is shared and
/// stops, and nothing of the first's run or lease is touched.
#[tokio::test(start_paused = true)]
async fn an_executor_that_finds_a_live_one_under_its_id_stops_without_touching_it() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    let mut twin = rig.restarted_executor();
    for _ in 0..(rig.cfg.adopt_watch().as_secs() / 5 + 3) {
        rig.advance(Duration::from_secs(5)).await;
        rig.exec.step().await;
        twin.step().await;
    }
    let why = twin.twin().expect("the twin did not stop");
    assert!(why.contains("same id"), "{why}");
    assert!(twin.hosts().is_empty());
    assert_eq!(rig.exec.hosts()[0].lease, lease, "the first lost its host");
    assert_eq!(count_tail(&rig.requests(), "DELETE", ""), 0);
    assert_eq!(count(&rig.requests(), "POST /v1/leases"), 1);
}

/// Catches the periodic check releasing a lease of another process under the same frontend id
/// as a stray: it is that process's, not a lease nobody holds.
#[tokio::test(start_paused = true)]
async fn the_stray_check_leaves_another_processs_lease_alone() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let resources = kbf_lease_client::Resources {
        cpu_millis: 2_000,
        memory_bytes: 16 * crate::rig::GIB,
        gpus: 0,
        gpu_class: None,
    };
    let request = kbf_lease_client::AcquireRequest::new("farm-linux", resources, rig.cfg.max_wall)
        .label("kbf-frontend", &rig.cfg.frontend)
        .label("kbf-instance", "another-process");
    let other = match rig.api.acquire(&request).await.expect("booked") {
        kbf_lease_client::Acquired::Granted(granted) => granted.id,
        kbf_lease_client::Acquired::Pending(pending) => pending.id,
    };
    rig.elapse(130).await;
    assert_eq!(state_of(&rig, other).await, Some(LeaseState::Held));
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{other}")),
        0
    );
}

/// Catches a lease of this very process that no slot holds (a booking whose answer was lost)
/// being taken for another frontend's: it is no rival, and the executor goes on.
#[tokio::test(start_paused = true)]
async fn a_lease_of_this_process_that_no_slot_holds_is_no_rival() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let stray = book_stray(&rig).await;
    assert_eq!(state_of(&rig, stray).await, Some(LeaseState::Held));
    assert_eq!(rig.exec.rival().await, None);
}

/// Books a lease under the frontend's label straight from the lease server, as a request
/// the executor sent and got no answer to may have been booked.
async fn book_stray(rig: &Rig) -> kbf_lease_client::LeaseId {
    let resources = kbf_lease_client::Resources {
        cpu_millis: 2_000,
        memory_bytes: 16 * crate::rig::GIB,
        gpus: 0,
        gpu_class: None,
    };
    let request = kbf_lease_client::AcquireRequest::new("farm-linux", resources, rig.cfg.max_wall)
        .label("kbf-frontend", &rig.cfg.frontend)
        .label("kbf-instance", &rig.cfg.instance);
    match rig.api.acquire(&request).await.expect("a stray lease") {
        kbf_lease_client::Acquired::Granted(granted) => granted.id,
        kbf_lease_client::Acquired::Pending(pending) => pending.id,
    }
}

async fn state_of(rig: &Rig, lease: kbf_lease_client::LeaseId) -> Option<LeaseState> {
    let leases = rig.api.leases().await.expect("leases");
    leases.iter().find(|l| l.id == lease).map(|l| l.state)
}

/// Catches a lease booked by a request whose answer was lost staying booked: the executor
/// does not know it, nothing renews or releases it, and it keeps a host until its ttl. After
/// an acquire that may have applied, the leases under the frontend's label are compared with
/// the slots' at once, and the one no slot holds is released.
#[tokio::test(start_paused = true)]
async fn a_lease_booked_by_a_lost_answer_is_released() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let first = rig.exec.hosts()[0].lease;
    rig.api.release(first).await.expect("released");
    rig.pump().await;
    rig.advance(Duration::from_secs(6)).await;
    rig.fake.inject_on("POST /v1/leases", Fault::Close);
    rig.exec.step().await;
    assert!(rig.exec.hosts().is_empty(), "the answer was lost");
    let stray = book_stray(&rig).await;
    rig.advance(Duration::from_secs(6)).await;
    rig.pump().await;
    assert_ne!(state_of(&rig, stray).await, Some(LeaseState::Held));
    let hosts = rig.exec.hosts();
    assert_eq!(hosts.len(), 1, "the slot was not filled again");
    assert_ne!(hosts[0].lease, stray);
    let released = format!("DELETE /v1/leases/{stray}");
    assert_eq!(count(&rig.requests(), &released), 1);
    let counted = rig.counted();
    assert!(
        counted.contains("kbf_executor_stray_leases_released_total 1\n"),
        "{counted}"
    );
}

/// Catches a lease the executor released without the lease server hearing (a flaky server
/// is when workers are retired) staying booked: the slot asks for a replacement at once and
/// on a pool sized to the machines is refused for room, with the first lease holding the
/// host until its ttl. The release is found out, and the lease released before the slot asks.
#[tokio::test(start_paused = true)]
async fn a_release_that_did_not_arrive_is_made_again() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| {
        short_leases(cfg);
        cfg.lease_ttl = Duration::from_secs(3600);
        cfg.host_check = Duration::from_secs(3600);
        cfg.reconcile_every = Duration::from_secs(3600);
    })
    .await;
    rig.pump().await;
    let first = rig.exec.hosts()[0].lease;
    // 181 s on, the lease is within 120 s of its end: the worker drains, and is released.
    rig.advance(Duration::from_secs(181)).await;
    rig.fake.inject_on("DELETE /v1/leases/", Fault::Close);
    rig.exec.step().await;
    assert_eq!(
        state_of(&rig, first).await,
        Some(LeaseState::Held),
        "the release arrived"
    );
    rig.advance(Duration::from_secs(1)).await;
    rig.pump().await;
    assert_ne!(state_of(&rig, first).await, Some(LeaseState::Held));
    let hosts = rig.exec.hosts();
    assert_eq!(hosts.len(), 1, "no replacement: the host was still booked");
    assert_ne!(hosts[0].lease, first);
}

/// Catches a stray lease no failure announced (the leases of a frontend that crashed
/// after booking, of a request that timed out and was read as a refusal) staying booked for
/// good: the leases under the label are compared with the slots' every `reconcile_every`.
#[tokio::test(start_paused = true)]
async fn a_stray_lease_is_found_by_the_periodic_check() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |cfg| {
        cfg.reconcile_every = Duration::from_secs(30);
    })
    .await;
    rig.pump().await;
    let held = rig.exec.hosts()[0].lease;
    let stray = book_stray(&rig).await;
    rig.advance(Duration::from_secs(10)).await;
    rig.pump().await;
    assert_eq!(state_of(&rig, stray).await, Some(LeaseState::Held));
    rig.advance(Duration::from_secs(25)).await;
    // The list that looks for strays fails once: it is made again at the next pass.
    rig.fake.inject_on("GET /v1/leases", Fault::Close);
    rig.exec.step().await;
    assert_eq!(state_of(&rig, stray).await, Some(LeaseState::Held));
    rig.pump().await;
    assert_ne!(state_of(&rig, stray).await, Some(LeaseState::Held));
    assert_eq!(state_of(&rig, held).await, Some(LeaseState::Held));
    assert_eq!(rig.exec.hosts()[0].lease, held);
}

/// Catches the check for strays releasing a lease that is already being released: a lease
/// with a live run is `Releasing` until the run ends, no slot holds it, and the lease server
/// is not asked again.
#[tokio::test(start_paused = true)]
async fn the_check_for_strays_leaves_a_lease_being_released_alone() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |cfg| {
        cfg.reconcile_every = Duration::from_secs(30);
    })
    .await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let first = rig.exec.hosts()[0].lease;
    let run = rig.runs_of(1).await.remove(0);
    assert!(rig.fake.run_started(&run.run));
    // The host is lost: the worker stops the run and releases the lease, which stays
    // `Releasing` while the run is live.
    assert!(rig.fake.set_lost("node1", true));
    rig.elapse(15).await;
    assert_eq!(state_of(&rig, first).await, Some(LeaseState::Releasing));
    assert_ne!(
        rig.exec.hosts()[0].lease,
        first,
        "the worker was not replaced"
    );
    let asked = format!("DELETE /v1/leases/{first}");
    let before = count(&rig.requests(), &asked);
    rig.elapse(60).await;
    assert_eq!(count(&rig.requests(), &asked), before, "released again");
    assert_eq!(state_of(&rig, first).await, Some(LeaseState::Releasing));
}

/// Catches a restarted frontend that adopts a lease whose runs it could not list, or could not
/// stop: they keep the host's budget until their wall time ends and every `Start` is refused
/// for room until its window is gone. The lease is given up instead, and the slot asks for
/// another once it may.
#[tokio::test(start_paused = true)]
async fn adoption_that_could_not_stop_the_runs_gives_the_lease_up() {
    for (what, fault_on) in [("list", "GET /v1/leases/"), ("stop", "DELETE /v1/leases/")] {
        let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
        rig.pump().await;
        let _caller = rig.submit(1);
        rig.pump().await;
        assert_eq!(rig.runs_of(1).await.len(), 1);
        let lease = rig.exec.hosts()[0].lease;
        rig.exec = rig.restarted_executor();
        rig.exec.step().await;
        rig.watch_over().await;
        rig.fake.inject_on(fault_on, Fault::Close);
        rig.exec.step().await;
        assert!(
            rig.exec.hosts().is_empty(),
            "{what}: adopted with live runs"
        );
        let released = count(&rig.requests(), &format!("DELETE /v1/leases/{lease}"));
        assert_eq!(released, 1, "{what}: the lease was not given up");
        assert_ne!(
            state_of(&rig, lease).await,
            Some(LeaseState::Held),
            "{what}"
        );
    }
}

/// Catches a lease renewed on the cadence of the ttl that was asked, not of the one that was
/// granted: the lease server grants at most its own maximum (an hour), so a ttl asked for
/// beyond three times that would be renewed only after the lease had expired, and the host
/// and its runs lost.
#[tokio::test(start_paused = true)]
async fn the_renewal_follows_the_ttl_the_server_granted() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| {
        cfg.lease_ttl = Duration::from_secs(4 * 3600);
        cfg.max_wall = Duration::from_secs(12 * 3600);
        cfg.drain_before_end = Duration::from_secs(3 * 3600);
    })
    .await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    rig.elapse(1_300).await;
    let renewals = count(&rig.requests(), &format!("POST /v1/leases/{lease}/renew"));
    assert!(renewals >= 1, "not renewed after 1300 s of a 3600 s grant");
    assert_eq!(rig.exec.hosts()[0].lease, lease);
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
    assert!(rig.counted().contains("kbf_executor_leases_held 0\n"));
}

/// Catches a shutdown that leaves a booked lease behind or stops at a lease the server will
/// not release: a booked lease is released, a failed release does not stop the rest, and a
/// slot without a lease is nothing to release.
#[tokio::test(start_paused = true)]
async fn shutdown_releases_what_is_booked_and_goes_on_past_a_failure() {
    let mut rig = Rig::built(&["node1"], 2, 16, true, |_| {}).await;
    rig.pump().await;
    assert_eq!(rig.api.leases().await.expect("leases").len(), 1);
    rig.fake.inject(kbf_lease_client::fake::Fault::Close);
    rig.exec.shutdown().await;
    assert!(rig.exec.hosts().is_empty());
    assert_eq!(count(&rig.requests(), "DELETE /v1/leases/4-1"), 1);

    let mut rig = Rig::start(&["node1"], 2, |_| {}).await;
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1, "the second slot has no host");
    rig.exec.shutdown().await;
    assert!(rig.api.leases().await.expect("leases").is_empty());
}

/// Catches a first step that gives up for good when the lease server cannot be asked which
/// leases are its own: the question is asked again at the next step, and the pool is filled.
#[tokio::test(start_paused = true)]
async fn adoption_that_could_not_ask_is_asked_again() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.fake.inject(kbf_lease_client::fake::Fault::Close);
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    assert_eq!(count(&rig.requests(), "GET /v1/leases"), 2);
}

/// Catches a lease that is being released (its runs are still stopping) being adopted as a
/// host: a restarted frontend leaves it to finish, and asks for a host of its own, which is
/// not that busy one.
#[tokio::test(start_paused = true)]
async fn a_lease_being_released_is_not_adopted() {
    let mut rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let first = rig.exec.hosts()[0].lease;
    let run = rig.runs_of(1).await.remove(0);
    assert!(rig.fake.run_started(&run.run));
    rig.api.release(first).await.expect("released");
    assert_eq!(
        rig.api.leases().await.expect("leases")[0].state,
        LeaseState::Releasing
    );
    rig.exec = rig.restarted_executor();
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    assert_ne!(rig.exec.hosts()[0].lease, first);
    assert_eq!(rig.exec.hosts()[0].host, "node2");
}

/// Catches a shutdown that leaves the farm placing actions on a worker that is gone: the farm
/// only learns it from its silence a grace later, and requeues what it placed there. The farm
/// is told the worker takes no more work, so an action submitted afterwards is not placed on
/// it.
#[tokio::test(start_paused = true)]
async fn shutdown_tells_the_farm_the_worker_takes_no_more_work() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let placed = |rig: &Rig| {
        let look = rig.farm.update_look();
        let node = look.iter().find(|n| n.id.to_string() == "js:node1");
        node.map_or(0, |n| n.leases)
    };
    rig.exec.shutdown().await;
    let _caller = rig.submit(1);
    rig.farm.tick().await;
    crate::rig::settle().await;
    assert_eq!(placed(&rig), 0, "an action was placed on a gone worker");
}
