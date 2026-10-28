//! Node phases over the worker stream (`docs/design/worker-protocol.md#node-phases`):
//! a Mac daemon that switches its GUI session for a whole-machine lease says so in a
//! resent `Hello`, restarts with the userspace restart, and comes back as another
//! process naming the lease again; a node whose cleanup failed says it is quarantined.
//! The scheduler's timing (the switch grace) is `kbf-sched`'s tests; these check what
//! crosses the wire.

mod support;

use kbf_proto::worker::{Hello, LeaseId, NodePhase, Phase, daemon_message};
use support::{Cell, FakeDaemon, Job, done, hello_on, output, ran};

const MAC: &[(&str, &str)] = &[
    ("arch", "arm64"),
    ("os", "macos"),
    ("drivers", "native"),
    ("drivers", "native-whole-machine"),
];

/// An action that books a few cores, so the Mac has room for a second.
const PLAIN: &[(&str, &str)] = &[];

const WHOLE: &[(&str, &str)] = &[("OSFamily", "macos"), ("kbf-lease", "whole_machine")];

/// The Mac's `Hello` from daemon process `instance`, in `phase`.
fn mac(instance: &str, phase: Option<NodePhase>) -> Hello {
    Hello {
        instance_id: instance.to_owned(),
        phase,
        ..hello_on("mac-1", 12, 32, MAC)
    }
}

/// Writes the server's log to the test's output at every level, so the arguments of
/// its log lines run.
fn log() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_test_writer()
        .try_init();
}

fn switching(phase: Phase, lease: LeaseId) -> Option<NodePhase> {
    Some(NodePhase {
        phase: phase.into(),
        lease_id: Some(lease),
        reason: String::new(),
    })
}

/// Catches a server that reads no phase from `Hello` (on a stream's first one or a
/// resent one), or does not send a switching lease's `Start` again to the daemon that
/// comes back after the userspace restart: that daemon is another process, so without
/// it the lease would wait for a `Start` that never comes and be given up. The lease
/// keeps its id (it is not granted again) and its `Result` is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_back_from_the_session_switch_is_sent_its_start_again() {
    log();
    let cell = Cell::start().await;
    let mut before = FakeDaemon::connect(cell.worker_addr, mac("before", None))
        .await
        .expect("registered");
    let job = Job::new("a GPU test", WHOLE);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let start = before.start().await;
    assert_eq!(start.kind, "whole_machine");
    let lease = start.lease_id.expect("a lease id");
    before.send(daemon_message::Message::Hello(mac(
        "before",
        switching(Phase::Preparing, lease),
    )));
    assert!(before.heartbeat(&[lease]).await);
    before.close();

    let preparing = mac("after the restart", switching(Phase::Preparing, lease));
    let mut after = FakeDaemon::connect(cell.worker_addr, preparing)
        .await
        .expect("registered");
    let again = after.start().await;
    assert_eq!(again.lease_id, Some(lease), "granted again, not resent");
    assert_eq!(again.kind, "whole_machine");
    assert_eq!(again.action_digest, start.action_digest);
    assert_eq!(
        (again.millicpus, again.memory_bytes),
        (start.millicpus, start.memory_bytes)
    );
    assert!(after.heartbeat(&[lease]).await);
    let result = output(&cell, "the GPU test ran", 0).await;
    assert!(after.report(ran(Some(lease), &result)).await.accepted);
    done(&mut ops).await;
}

/// Catches a switching lease the server does not hold that is left alone: a daemon
/// back from a restart past the switch grace (or one of a lease that finished) would
/// keep the Mac in that phase, its lease user logged in, with no `Start` ever coming.
/// The server cancels it, so the daemon cleans up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switching_lease_not_held_here_is_cancelled() {
    log();
    let cell = Cell::start().await;
    let mut daemon = FakeDaemon::connect(cell.worker_addr, mac("one", None))
        .await
        .expect("registered");
    let job = Job::new("a finished test", WHOLE);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let result = output(&cell, "done", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    done(&mut ops).await;

    daemon.send(daemon_message::Message::Hello(mac(
        "one",
        switching(Phase::Restoring, lease),
    )));
    assert_eq!(daemon.cancelled().await, Some(lease));
}

/// Catches a quarantined node that is offered work (the defect "release the node
/// after a failed cleanup", seen from the server), a quarantine that is not taken on
/// the stream's first `Hello` before placement, and one that cannot be lifted: once
/// the node reports serving again, the queued work starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quarantined_node_is_offered_nothing_until_it_serves_again() {
    log();
    let cell = Cell::start().await;
    let quarantined = Some(NodePhase {
        phase: Phase::Quarantined.into(),
        lease_id: None,
        reason: "user-delete of lease 7.3 failed".to_owned(),
    });
    let job = Job::new("waits for the Mac", WHOLE);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let mut daemon = FakeDaemon::connect(cell.worker_addr, mac("one", quarantined))
        .await
        .expect("registered");
    daemon.no_work().await;

    let serving = Some(NodePhase {
        phase: Phase::Serving.into(),
        ..NodePhase::default()
    });
    daemon.send(daemon_message::Message::Hello(mac("one", serving)));
    let start = daemon.start().await;
    let result = output(&cell, "ran once repaired", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;
}

/// Catches a node that says it is under memory pressure and is still offered work (the
/// phase read as serving, or not read at all), a pressure that is taken as a
/// quarantine (the node would be listed for an operator's attention), and one that
/// cannot be lifted: once the node reports serving again, the queued work starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_under_memory_pressure_is_offered_nothing_until_it_serves_again() {
    log();
    let cell = Cell::start().await;
    let pressured = Some(NodePhase {
        phase: Phase::MemoryPressure.into(),
        lease_id: None,
        reason: String::new(),
    });
    let job = Job::new("waits out the pressure", WHOLE);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let mut daemon = FakeDaemon::connect(cell.worker_addr, mac("one", pressured))
        .await
        .expect("registered");
    daemon.no_work().await;

    let serving = Some(NodePhase {
        phase: Phase::Serving.into(),
        ..NodePhase::default()
    });
    daemon.send(daemon_message::Message::Hello(mac("one", serving)));
    let start = daemon.start().await;
    let result = output(&cell, "ran once the pressure fell", 0).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    done(&mut ops).await;
}

/// Catches a preparing phase without its lease that is taken (the server could not
/// tell which lease to keep): the stream is refused at its first `Hello`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switching_phase_without_its_lease_is_refused() {
    let cell = Cell::start().await;
    let nameless = Some(NodePhase {
        phase: Phase::Preparing.into(),
        lease_id: None,
        reason: String::new(),
    });
    let refused = FakeDaemon::connect(cell.worker_addr, mac("one", nameless))
        .await
        .err()
        .expect("refused");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(refused.message().contains("names no lease"), "{refused:?}");
}

/// Catches a phase this server does not know (from a newer daemon) that is read as
/// serving: the node might be in no state to run work. It is held back like a
/// quarantine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_phase_is_offered_nothing() {
    let cell = Cell::start().await;
    let unknown = Some(NodePhase {
        phase: 99,
        ..NodePhase::default()
    });
    let mut daemon = FakeDaemon::connect(cell.worker_addr, mac("one", unknown))
        .await
        .expect("registered");
    let job = Job::new("waits", WHOLE);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    daemon.no_work().await;
}

/// Catches a tainted phase (it is on the wire, this server does not act on it yet) that
/// is read as serving: it is held back like a quarantine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tainted_phase_is_offered_nothing() {
    let cell = Cell::start().await;
    let tainted = Some(NodePhase {
        phase: Phase::Tainted.into(),
        reason: "a container could not be removed".to_owned(),
        ..NodePhase::default()
    });
    let mut daemon = FakeDaemon::connect(cell.worker_addr, mac("one", tainted))
        .await
        .expect("registered");
    let job = Job::new("waits", WHOLE);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    daemon.no_work().await;
}

/// Catches a draining daemon (it was told to stop and finishes its runs first) that is
/// still offered work (the phase read as serving: the restart drill's 18 s of
/// re-placing), or one whose running lease is lost for it: with the node heartbeating
/// and naming the lease, its Result is accepted, as any lease's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draining_node_is_offered_nothing_and_keeps_its_running_lease() {
    log();
    let cell = Cell::start().await;
    let mut daemon = FakeDaemon::connect(cell.worker_addr, mac("one", None))
        .await
        .expect("registered");
    let first = Job::new("runs through the drain", PLAIN);
    cell.upload(&first.blobs()).await;
    let mut ops = cell.execute(&first.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");

    let draining = Some(NodePhase {
        phase: Phase::Draining.into(),
        lease_id: None,
        reason: String::new(),
    });
    daemon.send(daemon_message::Message::Hello(mac("one", draining)));
    assert!(daemon.heartbeat(&[lease]).await);
    let second = Job::new("waits for another node", PLAIN);
    cell.upload(&second.blobs()).await;
    let _later = cell.execute(&second.action).await;
    daemon.no_work().await;
    assert!(daemon.heartbeat(&[lease]).await, "the lease is still held");

    let result = output(&cell, "finished while draining", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    done(&mut ops).await;
}
