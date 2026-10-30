//! What the executor does when the world breaks: the lease server restarts, a lease or a host
//! is lost, the farm ends the stream, an answer is lost, the farm gives a lease up.

use std::time::Duration;

use kbf_front::Finished;
use kbf_lease_client::Outcome;
use kbf_lease_client::fake::Fault;
use tonic::Status;

use crate::rig::{Rig, count, count_tail};

fn short_leases(cfg: &mut kbf_server::executor::Config) {
    cfg.max_run = Duration::from_secs(30);
    cfg.default_timeout = Duration::from_secs(30);
    cfg.run_margin = Duration::from_secs(20);
    cfg.lease_ttl = Duration::from_secs(60);
    cfg.drain_before_end = Duration::from_secs(120);
    cfg.max_wall = Duration::from_secs(300);
}

/// A rig with one action posted: its caller and its run.
async fn running(mut rig: Rig) -> (Rig, crate::rig::Caller, crate::rig::Posted) {
    rig.pump().await;
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
    (rig, caller, run)
}

/// Catches a lease-server restart that double-reports or loses an action: every run is
/// forgotten (the leases are kept), the run is reported lost once, the farm runs the action
/// again on the same lease, and the caller is answered by that second run alone.
#[tokio::test(start_paused = true)]
async fn a_lease_server_restart_that_keeps_the_leases_reruns_the_action_once() {
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    rig.fake.restart();
    assert!(rig.fake.host_hello("node1"));
    rig.pump().await;
    assert!(
        caller.finished().is_none(),
        "the lost run answered the caller"
    );
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_ne!(
        runs[0].run, first.run,
        "the forgotten run was taken for the new one"
    );
    assert_eq!(runs[0].lease, first.lease, "the lease was kept");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 1);
    assert_eq!(rig.exec.hosts()[0].runs, 0);
}

/// Catches a worker that goes on after its host fell silent to the lease server again: the
/// hosts say hello after a restart, and until they do the lease server refuses runs with
/// `host-not-registered`. The worker is taken down, its lease released, and the action runs
/// again on a worker of the lease that replaces it.
#[tokio::test(start_paused = true)]
async fn a_host_that_has_not_said_hello_is_given_up_and_replaced() {
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    rig.fake.restart();
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty(), "the host had not said hello");
    assert!(rig.fake.host_hello("node1"));
    rig.elapse(75).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_ne!(runs[0].lease, first.lease);
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a lease that the server no longer has (it expired, or an operator released it)
/// being waited on: the runs on it are reported lost at once, so the action runs again
/// without waiting for the farm's silence grace, and the slot is filled by a new lease.
#[tokio::test(start_paused = true)]
async fn a_vanished_lease_loses_its_runs_and_is_replaced() {
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    rig.api.release(first.lease).await.expect("released");
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty());
    assert!(caller.finished().is_none());
    rig.elapse(10).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_ne!(runs[0].lease, first.lease);
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// As above, found when the lease is renewed rather than when its runs are read.
#[tokio::test(start_paused = true)]
async fn a_lease_that_is_gone_at_its_renewal_is_replaced() {
    let (mut rig, _caller, first) = running(Rig::start(&["node1"], 1, short_leases).await).await;
    rig.api.release(first.lease).await.expect("released");
    rig.advance(Duration::from_secs(25)).await;
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty());
}

/// Catches a renewal that fails (the server answers 500) taking the worker down: it is tried
/// again, and the lease is kept.
#[tokio::test(start_paused = true)]
async fn a_failed_renewal_is_tried_again() {
    let (mut rig, _caller, first) = running(Rig::start(&["node1"], 1, short_leases).await).await;
    rig.advance(Duration::from_secs(25)).await;
    for _ in 0..2 {
        rig.fake.inject(Fault::Reply {
            status: 500,
            body: bytes::Bytes::from_static(b"{}"),
        });
    }
    rig.pump().await;
    assert_eq!(rig.exec.hosts()[0].lease, first.lease);
    rig.elapse(5).await;
    let renewals = count_tail(&rig.requests(), "POST", "/renew");
    assert!(renewals >= 2, "{renewals}");
}

/// Catches a lost host's runs outliving their retry: the lease server says the host is lost,
/// and the worker asks for its run to stop and falls silent (releasing its lease) at once. The
/// farm, hearing nothing, gives the lease up only after the scheduler's grace (60 s), and the
/// retry is posted after the stop, on another host.
#[tokio::test(start_paused = true)]
async fn a_lost_host_has_its_run_stopped_before_the_farm_requeues_it() {
    let (mut rig, caller, first) = running(Rig::start(&["node1", "node2"], 1, |_| {}).await).await;
    assert!(rig.fake.set_lost("node1", true));
    rig.advance(Duration::from_secs(6)).await;
    rig.pump().await;
    assert!(
        rig.exec.hosts().is_empty(),
        "the worker of a lost host went on"
    );
    let stop = format!("DELETE /v1/leases/{}/runs/{}", first.lease, first.run);
    assert_eq!(count(&rig.requests(), &stop), 1);
    assert!(caller.finished().is_none());
    assert_eq!(
        count_tail(&rig.requests(), "POST", "/runs"),
        1,
        "the retry was posted before the grace"
    );
    rig.elapse(75).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(
        runs.len(),
        1,
        "the farm did not run the action again: {runs:?}"
    );
    assert_eq!(runs[0].host, "node2");
    let requests = rig.requests();
    let stopped_at = requests.iter().position(|r| *r == stop).expect("a stop");
    let retried_at = requests
        .iter()
        .rposition(|r| r.ends_with("/runs") && r.starts_with("POST"))
        .expect("a post");
    assert!(
        stopped_at < retried_at,
        "the retry was posted before the stop"
    );
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a worker that keeps heartbeating a farm while it cannot reach the lease server, so
/// its runs may be lost for good with nobody asking: after the fence time (T, under the
/// scheduler's grace G) without an answer the worker is taken down and goes silent, so the
/// farm runs its leases again after G.
#[tokio::test(start_paused = true)]
async fn an_unreachable_lease_server_fences_the_worker_at_the_fence_time() {
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    for _ in 0..400 {
        rig.fake.inject(Fault::Close);
    }
    let mut seconds = 0;
    while !rig.exec.hosts().is_empty() {
        assert!(seconds <= 45, "not fenced after {seconds} s");
        rig.advance(Duration::from_secs(5)).await;
        seconds += 5;
        rig.pump().await;
    }
    assert!(seconds > 35, "fenced after {seconds} s: before T");
    assert!(seconds < 60, "fenced after {seconds} s: not before G");
    assert!(caller.finished().is_none());
    assert!(rig.fake.run_state(&first.run).is_some());
}

/// Catches a stream the farm ended (a handover sends a status in place of a message) being
/// treated as an error to retry, or ignored: the runs the farm gave up are stopped, the worker
/// registers again as a new process naming the old one stopped, and the farm runs the action
/// again at once on the new stream.
#[tokio::test(start_paused = true)]
async fn a_stream_the_farm_ended_has_its_runs_stopped_and_is_registered_again() {
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    assert_eq!(
        rig.farm
            .end_worker_streams(&Status::unavailable("handover")),
        1
    );
    rig.pump().await;
    let stop = format!("DELETE /v1/leases/{}/runs/{}", first.lease, first.run);
    assert_eq!(count(&rig.requests(), &stop), 1);
    let runs = rig.runs_of(1).await;
    assert_eq!(
        runs.len(),
        2,
        "the action was not run again on the new stream: {runs:?}"
    );
    assert_eq!(runs[0].lease, runs[1].lease, "the lease was kept");
    rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches the lease the farm gave up being run to its end beside its retry: the worker's
/// heartbeat lists a lease the farm no longer holds, the farm answers with a `Cancel`, and the
/// run is stopped; the late result is never reported.
#[tokio::test(start_paused = true)]
async fn a_lease_the_farm_gave_up_is_cancelled_and_its_run_stopped() {
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    // The executor is not stepped for longer than the grace: the farm gives the lease up
    // and places the action again.
    rig.advance(Duration::from_secs(65)).await;
    rig.farm.tick().await;
    rig.pump().await;
    rig.elapse(10).await;
    let stop = format!("DELETE /v1/leases/{}/runs/{}", first.lease, first.run);
    assert!(
        count(&rig.requests(), &stop) >= 1,
        "the old run was not stopped"
    );
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_ne!(runs[0].run, first.run);
    // The first run's late result, if its host still sends one, is not reported.
    let _ = rig
        .fake
        .finish_run(&first.run, Outcome::Succeeded, Some(0), None);
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 1);
}

/// Catches a post whose answer was lost being posted again (the run is queued, and a second
/// run of the action would run beside it): the worker cannot know, so it asks for its lease
/// to be released (which stops anything queued), is taken down, and the farm gives the lease
/// up and runs the action on a new lease.
#[tokio::test(start_paused = true)]
async fn a_post_whose_answer_was_lost_is_not_posted_again() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let lease = rig.exec.hosts()[0].lease;
    let caller = rig.submit(1);
    rig.fake.inject(Fault::Reply {
        status: 202,
        body: bytes::Bytes::from_static(b"not json"),
    });
    rig.pump().await;
    assert!(
        rig.exec.hosts().is_empty(),
        "the worker went on after an unknown post"
    );
    assert_eq!(
        count(&rig.requests(), &format!("DELETE /v1/leases/{lease}")),
        1
    );
    assert_eq!(
        count_tail(&rig.requests(), "POST", "/runs"),
        1,
        "posted again"
    );
    rig.elapse(75).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a host that reports a taint between two reads of the host list failing the action
/// or going on: the post is refused with `host-tainted`, the action is rerun (the farm's
/// fault), and the worker stops taking work.
#[tokio::test(start_paused = true)]
async fn a_post_refused_for_a_taint_reruns_the_action_and_drains_the_worker() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    assert!(rig.fake.taint_host("node1", "post-job-check"));
    let caller = rig.submit(1);
    rig.pump().await;
    assert!(rig.exec.hosts()[0].draining);
    assert!(rig.runs().await.is_empty());
    assert!(caller.finished().is_none());
}

/// Catches a post to a host the lease server lost being retried on that host: the worker is
/// taken down silently, and its lease released.
#[tokio::test(start_paused = true)]
async fn a_post_refused_for_a_lost_host_takes_the_worker_down() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    assert!(rig.fake.set_lost("node1", true));
    let _caller = rig.submit(1);
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty());
    assert!(rig.runs().await.is_empty());
}

/// Catches a lease that is gone being posted to anyway, or its refusal being taken for the
/// action's: the lease server answers `not-held` (or `unknown-lease`), the worker is taken
/// down, and the action is reported lost so the farm runs it again on the lease that replaces
/// this one.
#[tokio::test(start_paused = true)]
async fn a_post_to_a_lease_that_is_gone_loses_the_start_and_replaces_the_lease() {
    for renewing_first in [false, true] {
        let mut rig = Rig::start(&["node1"], 1, |cfg| {
            if renewing_first {
                // A ttl that only just covers the longest run: the lease is renewed
                // before a run is posted, and that renewal finds the lease gone.
                cfg.lease_ttl = cfg.max_run + cfg.run_margin + cfg.heartbeat * 2;
                cfg.drain_before_end = cfg.lease_ttl + Duration::from_secs(60);
            }
        })
        .await;
        rig.pump().await;
        let lease = rig.exec.hosts()[0].lease;
        rig.api.release(lease).await.expect("released");
        rig.advance(Duration::from_secs(1)).await;
        rig.inputs
            .facts
            .lock()
            .expect("facts")
            .as_mut()
            .expect("ok")
            .timeout = Some(rig.cfg.max_run);
        let caller = rig.submit(1);
        rig.pump().await;
        assert!(
            rig.exec.hosts().is_empty(),
            "renewing first: {renewing_first}"
        );
        rig.elapse(10).await;
        let runs = rig.runs_of(1).await;
        assert_eq!(runs.len(), 1, "renewing first: {renewing_first}");
        assert_ne!(runs[0].lease, lease);
        rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
        assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    }
}

/// Catches a stream the farm dropped without a word (another stream registered as the same
/// worker) being taken for a quiet one: the runs are stopped, the worker registers again, and
/// the action, whose lease the farm gives up, runs again.
#[tokio::test(start_paused = true)]
async fn a_worker_registered_again_by_another_stream_registers_again_in_turn() {
    use kbf_sched::DaemonInstance;
    use kbf_server::farm::Reported;
    let (mut rig, caller, first) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, _inbox) = tokio::sync::mpsc::unbounded_channel();
    rig.farm.register(
        &kbf_types::WorkerId::new("js:node1"),
        DaemonInstance::new("another-process"),
        DaemonInstance::new(""),
        Reported::serving(kbf_types::Resources::new(2_000, 4 << 30), caps),
        outbound,
        kbf_proto::worker::ServerMessage::default(),
    );
    rig.pump().await;
    let stop = format!("DELETE /v1/leases/{}/runs/{}", first.lease, first.run);
    assert_eq!(count(&rig.requests(), &stop), 1);
    rig.elapse(75).await;
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    rig.host_finishes(&runs[1].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a run the lease server refuses for the job's own sake (a 422) being the action's
/// failure or being posted again unchanged: it is infrastructure, so the farm runs the action
/// again.
#[tokio::test(start_paused = true)]
async fn a_refused_post_is_rerun() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.fake.inject(Fault::Reply {
        status: 422,
        body: bytes::Bytes::from_static(br#"{"error":"invalid-job"}"#),
    });
    rig.pump().await;
    assert!(caller.finished().is_none());
    let runs = rig.runs_of(1).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    rig.host_finishes(&runs[0].run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches an ingest that takes long (a slow store) stopping the worker's heartbeats: the
/// farm then gives its lease up and runs the action again beside the one being read. The
/// read runs on its own, the worker goes on heartbeating, and the result is reported when
/// the read ends.
#[tokio::test(start_paused = true)]
async fn a_slow_ingest_does_not_stop_the_heartbeats() {
    let (mut rig, caller, run) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    rig.ingest
        .hold
        .store(true, std::sync::atomic::Ordering::SeqCst);
    rig.host_finishes(&run.run, Outcome::Succeeded, Some(0));
    rig.pump().await;
    rig.elapse(150).await;
    assert!(caller.finished().is_none());
    assert_eq!(rig.exec.hosts()[0].runs, 1);
    assert_eq!(
        rig.runs_of(1).await.len(),
        1,
        "the farm ran the action again"
    );
    rig.ingest.release.notify_one();
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches a read left running for a worker that is gone: it is abandoned with the worker.
#[tokio::test(start_paused = true)]
async fn the_read_of_a_worker_that_goes_is_abandoned() {
    let (mut rig, caller, run) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    rig.ingest
        .hold
        .store(true, std::sync::atomic::Ordering::SeqCst);
    rig.host_finishes(&run.run, Outcome::Succeeded, Some(0));
    rig.pump().await;
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 1);
    assert!(rig.fake.set_lost("node1", true));
    rig.advance(Duration::from_secs(6)).await;
    rig.pump().await;
    assert!(rig.exec.hosts().is_empty());
    assert!(caller.finished().is_none());
}

/// Catches a lease server that accepts a request and never answers holding the executor's
/// pass for the client's own 30 s a call: a pass makes several, one after another, with the
/// heartbeats to the farm among them, and the farm requeues a worker it has not heard from
/// after a minute. Each call gives up after the call timeout, the pass ends, and the run goes
/// on being tracked.
#[tokio::test(start_paused = true)]
async fn a_lease_server_that_never_answers_does_not_hold_the_pass() {
    let (mut rig, caller, run) = running(Rig::start(&["node1"], 1, |_| {}).await).await;
    rig.fake.inject_on("GET /v1/leases/", Fault::Hang);
    let in_time = rig.step_through(Duration::from_secs(6)).await;
    assert!(in_time, "the pass waited for the client's own timeout");
    assert_eq!(rig.exec.hosts().len(), 1);
    rig.host_finishes(&run.run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
}

/// Catches the fence being judged by the clock at the start of the pass: a pass that spent
/// its time waiting for the lease server would then fence a pass late, with the lease server
/// silent for longer than the fence time. The clock is read where the silence is counted.
#[tokio::test(start_paused = true)]
async fn the_fence_counts_the_time_the_pass_itself_took() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| {
        cfg.fence_after = Duration::from_secs(10);
    })
    .await;
    rig.pump().await;
    assert_eq!(rig.exec.hosts().len(), 1);
    rig.advance(Duration::from_secs(9)).await;
    rig.fake.inject_on("GET /v1/leases/", Fault::Hang);
    // The pass starts 9 s after the lease server last answered, and waits 5 s for it.
    let in_time = rig.step_through(Duration::from_secs(6)).await;
    assert!(in_time);
    assert!(rig.exec.hosts().is_empty(), "the worker was not fenced");
}
