//! The daemon's drain on its first stop signal (`Daemon::run_draining`): it says it is
//! draining, takes no new lease, lets the leases it runs end and report, waits for
//! their acknowledgements and then returns (the process exits 0), however long that
//! takes. A deadline, if set, only logs; a second signal is the hard stop and ends it
//! at once.
//!
//! The signals are `()`s on a channel here, as `kbf-node` forwards SIGTERM and SIGINT.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::FakeRuntime;
use kbf_proto::google::rpc::Code;
use kbf_proto::worker::{Hello, NodePhase as WirePhase, Phase, daemon_message::Message};
use support::{Harness, PROMPT, Peer};

const RUN: Duration = Duration::from_millis(400);
const LONG: Duration = Duration::from_secs(60);
const QUIET: Duration = Duration::from_millis(300);
const SHORT: Duration = Duration::from_millis(100);

fn draining() -> Option<WirePhase> {
    Some(WirePhase {
        phase: Phase::Draining.into(),
        lease_id: None,
        reason: String::new(),
    })
}

/// The next Hello on `peer` within `within`.
async fn hello_again(peer: &mut Peer, within: Duration) -> Option<Hello> {
    peer.expect(within, |m| match m {
        Message::Hello(h) => Some(h.clone()),
        _ => None,
    })
    .await
    .map(|(_, hello)| hello)
}

/// A daemon (leases take `run_for`, drain deadline `deadline`) with a welcomed stream,
/// the signal sender and `n` leases `(1, 1)..=(1, n)` running.
async fn running(
    name: &str,
    run_for: Duration,
    deadline: Option<Duration>,
    n: u64,
) -> (Harness, tokio::sync::mpsc::UnboundedSender<()>, Peer) {
    let runtime = Arc::new(FakeRuntime::new(run_for));
    let (mut h, signal) = Harness::with_drain_deadline(name, runtime, deadline).await;
    let peer = h.welcomed().await;
    for seq in 1..=n {
        peer.start(1, seq, "action");
    }
    h.started(usize::try_from(n).expect("small")).await;
    (h, signal, peer)
}

/// Catches the drop-all (the restart drill's defect: 87 runs killed per node in two
/// seconds): on the first signal no running lease is killed, every one reports an OK
/// Result, the daemon does not return before the server has acknowledged all of them,
/// and returns once it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_signal_lets_every_lease_finish_report_and_be_acknowledged() {
    let (mut h, signal, mut peer) = running("drain-all", RUN, Some(LONG), 3).await;
    signal.send(()).expect("daemon runs");
    let hello = hello_again(&mut peer, PROMPT).await.expect("a new Hello");
    assert_eq!(hello.phase, draining(), "the node says it drains");

    let mut reported = Vec::new();
    for _ in 0..3 {
        let (_, result) = peer.result(PROMPT).await.expect("a Result per lease");
        let status = result.status.expect("a status");
        assert_eq!(status.code, Code::Ok as i32, "{status:?}");
        reported.push(result.lease_id.expect("a lease").seq);
    }
    reported.sort_unstable();
    assert_eq!(reported, [1, 2, 3]);
    assert!(h.runtime.killed().is_empty(), "no run was killed");
    assert!(
        !h.exited(QUIET).await,
        "the daemon returned before its Results were acknowledged"
    );
    for seq in 1..=2 {
        peer.ack_result(Some((1, seq)), true);
    }
    assert!(!h.exited(QUIET).await, "one Result is still unacknowledged");
    peer.ack_result(Some((1, 3)), true);
    assert!(
        h.exited(PROMPT).await,
        "everything acknowledged: it returns"
    );
    assert!(h.runtime.killed().is_empty());
}

/// Catches a draining node that still accepts a Start (the Start of a placement that
/// crossed the Hello on the wire): it must be refused with a Result the server runs
/// again (UNAVAILABLE), not started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_after_the_signal_is_refused_not_run() {
    let (h, signal, mut peer) = running("drain-start", LONG, Some(LONG), 1).await;
    signal.send(()).expect("daemon runs");
    hello_again(&mut peer, PROMPT).await.expect("draining");
    peer.start(1, 9, "action");
    let (_, refused) = peer.result(PROMPT).await.expect("a refusal");
    assert_eq!(refused.lease_id.expect("a lease").seq, 9);
    assert_eq!(
        refused.status.expect("a status").code,
        Code::Unavailable as i32
    );
    assert_eq!(
        h.runtime.started().len(),
        1,
        "the new lease was not started"
    );
}

/// Catches an uninterruptible drain: the second signal ends it at once while a lease
/// is still running, and kills that lease (the operator's hard stop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_signal_ends_the_drain_at_once() {
    for deadline in [None, Some(LONG)] {
        let (mut h, signal, mut peer) = running("drain-second", LONG, deadline, 1).await;
        signal.send(()).expect("daemon runs");
        hello_again(&mut peer, PROMPT).await.expect("draining");
        assert!(!h.exited(QUIET).await, "one signal drains and waits");
        signal.send(()).expect("daemon runs");
        assert!(h.exited(PROMPT).await, "the second signal stops it");
    }
}

/// Catches a deadline that abandons the lease it was meant to be patient with (the old
/// behaviour: the drain returned at the deadline, the process exited and the lease was
/// killed and requeued as a lost run). The deadline passes while the lease still runs;
/// the daemon goes on waiting, the lease finishes with an OK Result, nothing is killed,
/// and the drain ends once that Result is acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deadline_that_passes_does_not_abandon_the_lease() {
    let (mut h, signal, mut peer) =
        running("drain-deadline", Duration::from_secs(1), Some(SHORT), 1).await;
    signal.send(()).expect("daemon runs");
    hello_again(&mut peer, PROMPT).await.expect("draining");
    assert!(
        !h.exited(Duration::from_millis(400)).await,
        "the daemon returned at its deadline with a lease running"
    );
    let (_, result) = peer.result(PROMPT).await.expect("the lease finishes");
    assert_eq!(result.status.expect("a status").code, Code::Ok as i32);
    assert!(h.runtime.killed().is_empty(), "a lease was killed");
    assert!(!h.exited(QUIET).await, "the Result is not acknowledged yet");
    peer.ack_result(Some((1, 1)), true);
    assert!(h.exited(PROMPT).await, "acknowledged: the drain ends");
}

/// Catches a default deadline: with none set the drain waits on a running lease, ends
/// when the lease has reported and been acknowledged, and kills nothing. (The wait past
/// hours is proved on a paused clock in `kbf-daemon`'s `drain` unit tests.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_deadline_the_drain_ends_when_the_lease_does() {
    let runtime = Arc::new(FakeRuntime::new(Duration::from_secs(1)));
    let (mut h, signal) = Harness::with_drain_deadline("drain-none", runtime, None).await;
    let mut peer = h.welcomed().await;
    peer.start(1, 1, "action");
    h.started(1).await;
    signal.send(()).expect("daemon runs");
    hello_again(&mut peer, PROMPT).await.expect("draining");
    assert!(!h.exited(QUIET).await, "it stopped with a lease running");
    let (_, result) = peer.result(PROMPT).await.expect("the lease finishes");
    assert_eq!(result.status.expect("a status").code, Code::Ok as i32);
    peer.ack_result(Some((1, 1)), true);
    assert!(h.exited(PROMPT).await, "acknowledged: the drain ends");
    assert!(h.runtime.killed().is_empty());
}

/// Catches a deadline of 0 that still means "stop at once" (what 0 meant before): it
/// only reports at once, and the drain waits for the lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_deadline_does_not_end_the_drain() {
    let (mut h, signal, mut peer) = running(
        "drain-zero",
        Duration::from_secs(1),
        Some(Duration::ZERO),
        1,
    )
    .await;
    signal.send(()).expect("daemon runs");
    hello_again(&mut peer, PROMPT).await.expect("draining");
    assert!(!h.exited(QUIET).await, "0 stopped the drain");
    peer.result(PROMPT).await.expect("the lease finishes");
    peer.ack_result(Some((1, 1)), true);
    assert!(h.exited(PROMPT).await);
    assert!(h.runtime.killed().is_empty());
}

/// Catches a node with nothing to wait for that waits anyway: with no lease and no
/// unacknowledged Result the first signal returns at once, connected or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_node_returns_at_the_first_signal_connected_or_not() {
    let runtime = Arc::new(FakeRuntime::new(RUN));
    let (mut h, signal) = Harness::with_drain("drain-idle", runtime, LONG).await;
    let _peer = h.welcomed().await;
    signal.send(()).expect("daemon runs");
    assert!(h.exited(PROMPT).await, "idle and connected");

    let runtime = Arc::new(FakeRuntime::new(RUN));
    let (mut h, signal) = Harness::with_drain("drain-idle-off", runtime, LONG).await;
    h.hold_connections(true);
    tokio::time::sleep(QUIET).await;
    signal.send(()).expect("daemon runs");
    assert!(h.exited(PROMPT).await, "idle and with no stream");
}

/// Catches a drain forgotten by a new stream: a reconnect while draining must say
/// draining in its first Hello (else the server offers the node leases again), and the
/// Result that was not acknowledged is resent there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_stream_while_draining_says_draining_and_resends_the_result() {
    let (mut h, signal, mut first) = running("drain-stream", RUN, Some(LONG), 1).await;
    signal.send(()).expect("daemon runs");
    first.result(PROMPT).await.expect("a Result");
    first.close();
    let mut next = h.session().await;
    assert_eq!(next.hello().await.phase, draining());
    next.welcome();
    let (_, resent) = next.result(PROMPT).await.expect("resent");
    assert_eq!(resent.lease_id.expect("a lease").seq, 1);
    next.ack_result(Some((1, 1)), true);
    assert!(h.exited(PROMPT).await);
}

/// Catches a daemon that takes a closed signal channel for a signal: with no sender
/// left, nothing asks it to stop, so it runs on (a test harness dropping its sender, or
/// a forwarder that ended, must not drain or stop the node).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_signal_channel_is_no_signal() {
    let runtime = Arc::new(FakeRuntime::new(RUN));
    let (mut h, signal) = Harness::with_drain("drain-closed", runtime, LONG).await;
    let _peer = h.welcomed().await;
    drop(signal);
    assert!(!h.exited(QUIET).await, "the daemon stopped with no signal");
}

/// Catches a drain that overwrites a phase the runtime reports: a node quarantined (or
/// switching a session) keeps saying so while it drains, since the server reads that
/// phase to keep or fence its lease. Draining replaces serving only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draining_keeps_a_phase_the_runtime_reports() {
    let (phase_tx, phase_rx) =
        tokio::sync::watch::channel(kbf_types::NodePhase::Quarantined("scan dirty".to_owned()));
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, signal) =
        Harness::with_drain_phase("drain-phase", runtime, Some(LONG), Some(phase_rx)).await;
    let mut peer = h.session().await;
    let first = peer.hello().await;
    peer.welcome();
    peer.start(1, 1, "action");
    h.started(1).await;
    signal.send(()).expect("daemon runs");
    assert!(
        hello_again(&mut peer, QUIET).await.is_none(),
        "the phase is unchanged: no new Hello"
    );
    assert_eq!(
        first.phase.expect("a phase").phase,
        Phase::Quarantined as i32
    );
    drop(phase_tx);
    signal.send(()).expect("daemon runs");
    assert!(h.exited(PROMPT).await);
}

/// Catches `Daemon::run` (the immediate stop the combined modes still use) that ignores
/// its shutdown future: it returns when the future completes, with a lease still
/// running, which is abandoned, not waited for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_stops_at_once_when_its_shutdown_completes() {
    let runtime = Arc::new(FakeRuntime::new(LONG));
    let (mut h, stop) = Harness::with_shutdown("run-shutdown", runtime).await;
    let peer = h.welcomed().await;
    peer.start(1, 1, "action");
    h.started(1).await;
    assert!(!h.exited(QUIET).await, "it stopped before its shutdown");
    stop.send(()).expect("daemon runs");
    assert!(h.exited(PROMPT).await, "a running lease held it back");
}
