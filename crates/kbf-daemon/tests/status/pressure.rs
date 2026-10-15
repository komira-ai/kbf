//! The memory backstop (`docs/design/worker-protocol.md#node-phases`, `PHASE_MEMORY_
//! PRESSURE`): the pressure file's `some avg10`, the rule that pauses a node, and the
//! daemon's Hello while it is paused.
//!
//! Part of the `status` test binary, for the reason `phase` is: the session loop's
//! arms are counted from the binary that runs most of them.

use std::sync::Arc;
use std::time::Duration;

use super::support::{Harness, PROMPT, Peer, scratch};
use kbf_daemon::FakeRuntime;
use kbf_daemon::pressure::{paused, some_avg10, watch};
use kbf_proto::worker::{Hello, NodePhase as WirePhase, Phase, daemon_message};
use kbf_types::NodePhase;
use tokio::sync::watch as tokio_watch;

const KERNEL: &str = "some avg10=12.34 avg60=3.00 avg300=1.00 total=123456\n\
                      full avg10=99.00 avg60=0.00 avg300=0.00 total=654321\n";

/// Catches the wrong line or field read: the `full` line's `avg10` (99), `avg60` or
/// `avg300` for `avg10`; a file with no `some` line, no `avg10` field or a value that
/// is not a number (or not finite) read as a pressure.
#[test]
fn the_some_line_s_avg10_is_the_pressure() {
    assert_eq!(some_avg10(KERNEL), Some(12.34));
    assert_eq!(
        some_avg10("full avg10=1.00 avg60=0.00 avg300=0.00 total=0\n"),
        None
    );
    assert_eq!(some_avg10("some avg60=3.00 avg300=1.00 total=1\n"), None);
    assert_eq!(some_avg10("some avg10=lots avg60=3.00\n"), None);
    assert_eq!(some_avg10("some avg10=NaN avg60=3.00\n"), None);
    assert_eq!(some_avg10("some avg10=inf avg60=3.00\n"), None);
    assert_eq!(some_avg10(""), None);
    assert_eq!(some_avg10("somewhere avg10=50.00\n"), None);
}

/// Catches a pause at or below the threshold (`>=`), one with no lease running (the
/// node would stay idle for good under pressure its leases do not cause: the deadlock
/// the backstop must not have), a pause that ignores the reading, and one on no
/// reading.
#[test]
fn a_node_pauses_only_above_the_threshold_and_only_while_a_lease_runs() {
    assert!(paused(Some(10.01), 10.0, 1));
    assert!(paused(Some(100.0), 10.0, 3));
    assert!(!paused(Some(10.0), 10.0, 1), "at the threshold");
    assert!(!paused(Some(9.99), 10.0, 1));
    assert!(!paused(Some(0.0), 10.0, 1));
    assert!(!paused(Some(80.0), 10.0, 0), "no lease runs: serve");
    assert!(!paused(None, 10.0, 1), "no reading");
}

/// Catches a sampler that never reads the file (the receiver would hold its first
/// value for ever), one that reads the wrong line, one that keeps a value after the
/// file is gone (a node would stay paused on a stale reading), one that does not see
/// the file come back, and one that outlives its receiver.
#[tokio::test]
async fn the_sampler_follows_the_file_and_stops_with_its_receiver() {
    let dir = scratch("psi-sample");
    let file = dir.join("memory");
    assert_eq!(
        *watch(file.clone(), Duration::from_millis(10)).borrow(),
        None
    );
    std::fs::write(&file, KERNEL).expect("write");
    let mut rx = watch(file.clone(), Duration::from_millis(10));
    assert_eq!(*rx.borrow(), Some(12.34), "the first sample is at once");
    std::fs::write(&file, "some avg10=40.00 avg60=0.00 avg300=0.00 total=1\n").expect("write");
    tokio::time::timeout(PROMPT, rx.wait_for(|v| *v == Some(40.0)))
        .await
        .expect("a new reading in time")
        .expect("the sampler runs");
    std::fs::remove_file(&file).expect("remove");
    tokio::time::timeout(PROMPT, rx.wait_for(Option::is_none))
        .await
        .expect("the missing file in time")
        .expect("the sampler runs");
    std::fs::write(&file, KERNEL).expect("write");
    tokio::time::timeout(PROMPT, rx.wait_for(|v| *v == Some(12.34)))
        .await
        .expect("the file back in time")
        .expect("the sampler runs");
    // With no receiver left the sampler ends: nothing to observe from here but a
    // clean drop (a sampler that never ended would be a task per stopped daemon).
    drop(rx);
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn memory_pressure() -> Option<WirePhase> {
    Some(WirePhase {
        phase: Phase::MemoryPressure.into(),
        lease_id: None,
        reason: String::new(),
    })
}

/// The next resent Hello on `peer`, within `within`.
async fn resent(peer: &mut Peer, within: Duration) -> Option<Hello> {
    peer.expect(within, |m| match m {
        daemon_message::Message::Hello(h) => Some(h.clone()),
        _ => None,
    })
    .await
    .map(|(_, hello)| hello)
}

const QUIET: Duration = Duration::from_millis(300);

/// A daemon whose fake runtime keeps each lease for a minute, with a pressure the test
/// moves and a threshold of 10 percent; its first Hello, and the stream.
async fn paused_daemon(
    name: &str,
    stall: f64,
    phase: Option<tokio_watch::Receiver<NodePhase>>,
) -> (Harness, tokio_watch::Sender<Option<f64>>, Peer, Hello) {
    paused_daemon_for(name, stall, phase, Duration::from_secs(60)).await
}

/// [`paused_daemon`], each lease taking `run_for`.
async fn paused_daemon_for(
    name: &str,
    stall: f64,
    phase: Option<tokio_watch::Receiver<NodePhase>>,
    run_for: Duration,
) -> (Harness, tokio_watch::Sender<Option<f64>>, Peer, Hello) {
    let (tx, rx) = tokio_watch::channel(Some(stall));
    let runtime = Arc::new(FakeRuntime::new(run_for));
    let mut h = Harness::with_memory_backstop(name, runtime, rx, 10.0, phase).await;
    let mut peer = h.session().await;
    let hello = peer.hello().await;
    peer.welcome();
    (h, tx, peer, hello)
}

/// Catches, with the real session loop: a node that runs a lease under pressure and
/// does not say so (the server would place work on it), one that says so with no
/// lease, one that resends for a reading that changes nothing, one that does not say
/// serving once the pressure falls, and one that stays paused once its last lease has
/// ended while the pressure is still high (nothing would ever run on it again).
#[tokio::test]
async fn a_node_under_pressure_with_a_lease_stops_taking_leases_and_resumes() {
    // High pressure and no lease: serving, and still serving once the first one runs
    // and ends.
    let (h, tx, mut peer, hello) = paused_daemon("psi-hello", 50.0, None).await;
    assert_eq!(hello.phase, None, "no lease runs: serve");
    assert!(resent(&mut peer, QUIET).await.is_none());

    // A lease starts: now it is paused, and the Hello says so at once.
    peer.start(1, 1, "action");
    h.started(1).await;
    let hello = resent(&mut peer, PROMPT).await.expect("paused");
    assert_eq!(hello.phase, memory_pressure());
    // Another reading above the threshold changes nothing.
    tx.send_replace(Some(60.0));
    assert!(resent(&mut peer, QUIET).await.is_none());
    // The pressure falls: serving again, with no phase.
    tx.send_replace(Some(4.0));
    let hello = resent(&mut peer, PROMPT).await.expect("resumed");
    assert_eq!(hello.phase, None);
    // And rises again, and then the lease is cancelled: with nothing running the node
    // serves, whatever the pressure.
    tx.send_replace(Some(70.0));
    let hello = resent(&mut peer, PROMPT).await.expect("paused again");
    assert_eq!(hello.phase, memory_pressure());
    peer.cancel(Some((1, 1)));
    let hello = resent(&mut peer, PROMPT).await.expect("no lease: serve");
    assert_eq!(hello.phase, None);
    assert_eq!(*tx.borrow(), Some(70.0), "the pressure is still high");
}

/// Catches a first Hello of a new stream that names the phase before the pause (a
/// reconnect would offer the node leases while it is paused), and no reading (a file
/// that cannot be read) pausing a node.
#[tokio::test]
async fn a_new_stream_says_the_node_is_paused_and_no_reading_never_pauses() {
    let (mut h, tx, mut peer, _) = paused_daemon("psi-stream", 50.0, None).await;
    peer.start(1, 1, "action");
    h.started(1).await;
    assert_eq!(
        resent(&mut peer, PROMPT).await.expect("paused").phase,
        memory_pressure()
    );
    peer.close();
    let mut next = h.session().await;
    assert_eq!(next.hello().await.phase, memory_pressure());
    next.welcome();

    tx.send_replace(None);
    let hello = resent(&mut next, PROMPT).await.expect("no reading: serve");
    assert_eq!(hello.phase, None);
}

/// Catches a pause kept from before the stream went down: the only lease ends while no
/// stream is up, the pressure is still high, and the next stream's first Hello must
/// say serving (a node that holds nothing and says paused is idle for good).
#[tokio::test]
async fn a_lease_that_ends_between_streams_does_not_leave_the_node_paused() {
    let (mut h, _tx, mut peer, _) =
        paused_daemon_for("psi-between", 50.0, None, Duration::from_millis(400)).await;
    peer.start(1, 1, "action");
    h.started(1).await;
    assert_eq!(
        resent(&mut peer, PROMPT).await.expect("paused").phase,
        memory_pressure()
    );
    h.hold_connections(true);
    peer.close();
    // The lease ends (its run takes 400 ms) with no stream to say so on: the daemon's
    // new connection waits.
    tokio::time::sleep(Duration::from_millis(900)).await;
    h.hold_connections(false);
    let mut next = h.session().await;
    assert_eq!(next.hello().await.phase, None);
}

/// Catches a pause that hides the runtime's own phase (a quarantined node would say
/// serving-but-paused and, once the pressure fell, serving), and one that is lost
/// when the runtime's phase returns to serving while the pressure is still high.
#[tokio::test]
async fn the_runtimes_phase_comes_before_the_pause() {
    let (phase, rx) = tokio_watch::channel(NodePhase::Serving);
    let (h, _tx, mut peer, _) = paused_daemon("psi-phase", 50.0, Some(rx)).await;
    peer.start(1, 1, "action");
    h.started(1).await;
    assert_eq!(
        resent(&mut peer, PROMPT).await.expect("paused").phase,
        memory_pressure()
    );
    phase.send_replace(NodePhase::Quarantined("clean failed".to_owned()));
    let hello = resent(&mut peer, PROMPT).await.expect("quarantined");
    assert_eq!(
        hello.phase.map(|p| (p.phase, p.reason)),
        Some((Phase::Quarantined.into(), "clean failed".to_owned()))
    );
    phase.send_replace(NodePhase::Serving);
    let hello = resent(&mut peer, PROMPT).await.expect("paused again");
    assert_eq!(hello.phase, memory_pressure());
}
