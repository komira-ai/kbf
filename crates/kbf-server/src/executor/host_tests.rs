use std::time::Duration;

use kbf_proto::worker::{Cancel, ResultAck, ServerMessage, server_message};
use kbf_types::LeaseId;
use tokio::time::Instant;

use super::HostWorker;
use crate::executor::View;
use crate::executor::runs::Stage;
use crate::executor::test_support::{GIB, Harness, start};

fn message(message: server_message::Message) -> Result<ServerMessage, tonic::Status> {
    Ok(ServerMessage {
        message: Some(message),
    })
}

fn stage_of(worker: &HostWorker, lease: LeaseId) -> &'static str {
    match worker.tasks.get(&lease).map(|task| &task.stage) {
        None => "none",
        Some(Stage::Waiting(_)) => "waiting",
        Some(Stage::Running(_)) => "running",
        Some(Stage::Ingesting(_)) => "ingesting",
        Some(Stage::Reporting(_)) => "reporting",
    }
}

/// Catches a `Start` acted on that names a heartbeat the worker never sent (a daemon refuses
/// it: it may be a copy from another stream), and one with no window (a server that sent none
/// bounds nothing, which here is read as no time at all, the safe way): neither is posted, and
/// neither is kept.
#[tokio::test(start_paused = true)]
async fn a_start_outside_what_it_names_is_dropped_unposted() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker().await;
    let (unknown_beat, no_window) = (
        kbf_proto::worker::Start {
            heartbeat_seq: 99,
            ..start(1, 1)
        },
        kbf_proto::worker::Start {
            valid_for_ms: 0,
            ..start(1, 2)
        },
    );
    messages
        .send(message(server_message::Message::Start(unknown_beat)))
        .expect("sent");
    messages
        .send(message(server_message::Message::Start(no_window)))
        .expect("sent");
    let flow = worker
        .step(&harness.ctx(), &View::default(), Instant::now())
        .await;
    assert_eq!(flow, None);
    assert!(worker.tasks.is_empty());
    let runs = harness.deps.client.runs(worker.lease).await.expect("runs");
    assert!(runs.is_empty());
}

/// Catches a `Cancel` that leaves a run going or does not forget its lease: a running run is
/// asked to stop and the task forgotten once the lease server has answered, any other task
/// (waiting, or not a lease of this worker at all) is forgotten at once.
#[tokio::test(start_paused = true)]
async fn a_cancel_stops_a_run_and_forgets_the_lease() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker().await;
    let (ctx, view) = (harness.ctx(), View::default());
    messages
        .send(message(server_message::Message::Start(start(1, 1))))
        .expect("sent");
    messages
        .send(message(server_message::Message::Start(start(1, 2))))
        .expect("sent");
    worker.step(&ctx, &view, Instant::now()).await;
    assert_eq!(stage_of(&worker, LeaseId::new(1, 1)), "running");
    let run = harness.deps.client.runs(worker.lease).await.expect("runs")[0]
        .run_id
        .clone();
    let cancel = |seq| {
        message(server_message::Message::Cancel(Cancel {
            lease_id: Some(kbf_proto::worker::LeaseId { term: 1, seq }),
        }))
    };
    messages.send(cancel(1)).expect("sent");
    messages.send(cancel(77)).expect("sent");
    worker.step(&ctx, &view, Instant::now()).await;
    assert_eq!(stage_of(&worker, LeaseId::new(1, 1)), "none");
    assert_eq!(
        stage_of(&worker, LeaseId::new(1, 2)),
        "running",
        "another lease was cancelled"
    );
    let stop = format!("DELETE /v1/leases/{}/runs/{run}", worker.lease);
    assert_eq!(
        harness
            .fake
            .requests()
            .iter()
            .filter(|r| **r == stop)
            .count(),
        1
    );
}

/// Catches an acknowledgement that does not release the result it answers, or one that
/// releases another: only the acknowledged lease is forgotten, and a message that names no lease
/// (or a lease the worker does not have) forgets nothing.
#[tokio::test(start_paused = true)]
async fn an_acknowledgement_forgets_its_lease_only() {
    let harness = Harness::new().await;
    let (mut worker, _messages) = harness.worker().await;
    for seq in [1, 2] {
        worker.accept(&start(1, seq));
    }
    let ack = |lease_id| {
        message(server_message::Message::ResultAck(ResultAck {
            lease_id,
            accepted: true,
            reason: 0,
        }))
        .expect("a message")
    };
    worker.on_message(ack(None));
    worker.on_message(ack(Some(kbf_proto::worker::LeaseId { term: 1, seq: 9 })));
    assert_eq!(worker.tasks.len(), 2);
    worker.on_message(ack(Some(kbf_proto::worker::LeaseId { term: 1, seq: 1 })));
    assert_eq!(stage_of(&worker, LeaseId::new(1, 1)), "none");
    assert_eq!(stage_of(&worker, LeaseId::new(1, 2)), "waiting");
}

/// Catches a heartbeat the farm does not acknowledge counting as one it did: the fence time
/// is counted from the newest acknowledged heartbeat, so a worker the farm has stopped
/// hearing must not move it.
#[tokio::test(start_paused = true)]
async fn an_unacknowledged_heartbeat_does_not_move_the_fence() {
    let harness = Harness::new().await;
    let (mut worker, _messages) = harness.worker().await;
    let ctx = harness.ctx();
    let registered = Instant::now();
    tokio::time::advance(Duration::from_secs(6)).await;
    // A stream of another worker: the farm takes no heartbeat on it.
    let other = harness.farm.register(
        &kbf_types::WorkerId::new("other"),
        kbf_sched::DaemonInstance::new("i"),
        kbf_sched::DaemonInstance::new(""),
        worker.reported.clone(),
        tokio::sync::mpsc::unbounded_channel().0,
        ServerMessage::default(),
    );
    worker.stream = other;
    let flow = worker.beat(&ctx).await;
    assert!(flow.is_continue());
    assert_eq!(
        worker.last_ack, registered,
        "an unacknowledged heartbeat moved the fence"
    );
    assert_eq!(worker.seq, 1);
}

/// Catches a `Start` that the farm sends twice for one lease posting two runs of it (the
/// second would run beside the first and report beside it): the second changes nothing, even
/// after the first was posted.
#[tokio::test(start_paused = true)]
async fn a_start_sent_twice_posts_one_run() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker().await;
    let (ctx, view) = (harness.ctx(), View::default());
    for _ in 0..2 {
        messages
            .send(message(server_message::Message::Start(start(1, 1))))
            .expect("sent");
    }
    worker.step(&ctx, &view, Instant::now()).await;
    messages
        .send(message(server_message::Message::Start(start(1, 1))))
        .expect("sent");
    worker.step(&ctx, &view, Instant::now()).await;
    assert_eq!(stage_of(&worker, LeaseId::new(1, 1)), "running");
    let runs = harness.deps.client.runs(worker.lease).await.expect("runs");
    assert_eq!(runs.len(), 1);
}

/// Catches a worker that forgets when it sent a heartbeat that a `Start` may still name: the
/// farm judges a `Start` from the heartbeat it names and gives it 14 s, so a count of sends
/// kept (16 of them, which at a heartbeat of 100 ms is 1.6 s) drops a legitimate `Start`
/// and costs the farm a whole grace to run it again. Sends are kept for the window, whatever
/// the interval, and no longer.
#[tokio::test(start_paused = true)]
async fn the_sends_remembered_cover_the_start_window_at_any_heartbeat_interval() {
    let mut harness = Harness::new().await;
    harness.cfg.heartbeat = Duration::from_millis(100);
    let (mut worker, _messages) = harness.worker().await;
    let ctx = harness.ctx();
    for _ in 0..139 {
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(worker.beat(&ctx).await.is_continue());
    }
    assert!(
        worker.sent_at(0).is_some(),
        "the registration was forgotten 13.9 s on"
    );
    tokio::time::advance(Duration::from_millis(100)).await;
    assert!(worker.beat(&ctx).await.is_continue());
    assert!(
        worker.sent_at(0).is_none(),
        "a send from outside the window was kept"
    );
    assert!(worker.sent_at(worker.seq).is_some());
}

/// A load as the lease server lists it, of `seq`.
fn listed(seq: u64) -> kbf_lease_client::HostLoad {
    kbf_lease_client::HostLoad {
        seq,
        mono_usec: seq * 1_000_000,
        cpu_usage_usec: seq * 500_000,
        memory_working_set_bytes: 2 * GIB,
        memory_active_file_bytes: 0,
        actions_cpu_limited: false,
        covered: Vec::new(),
        lease_clocks: Vec::new(),
        minutes: Vec::new(),
    }
}

/// The lease server's host list as one pass reads it.
async fn polled(harness: &Harness) -> View {
    View {
        leases: None,
        hosts: harness.deps.client.hosts().await.expect("hosts"),
    }
}

/// The working set the farm shows for the worker, if it holds a reading of it.
fn farm_working_set(harness: &Harness, worker: &HostWorker) -> Option<u64> {
    harness
        .farm
        .node_loads()
        .into_iter()
        .find(|(id, _)| *id == worker.id)
        .map(|(_, load)| load.memory_working_set_bytes)
}

/// Catches a host's listed load that never reaches the farm (the worker heartbeats with
/// none, as it did before the feed), and one that reaches it as another figure: the farm
/// holds the reading the lease server listed for the worker's own host.
#[tokio::test(start_paused = true)]
async fn the_load_the_host_reports_reaches_the_farm_with_the_heartbeat() {
    let harness = Harness::new().await;
    let (mut worker, _messages) = harness.worker().await;
    assert_eq!(farm_working_set(&harness, &worker), None);
    assert!(harness.fake.set_host_load("node1", Some(listed(5))));
    let view = polled(&harness).await;
    worker.step(&harness.ctx(), &view, Instant::now()).await;
    assert_eq!(farm_working_set(&harness, &worker), Some(2 * GIB));
}

/// Catches a zero invented for a host that reports nothing: a host listed with no load, and
/// a pass whose host list was not read, leave the farm with no reading at all.
#[tokio::test(start_paused = true)]
async fn a_host_with_no_load_gives_the_farm_none() {
    let harness = Harness::new().await;
    let (mut worker, _messages) = harness.worker().await;
    let ctx = harness.ctx();
    let view = polled(&harness).await;
    worker.step(&ctx, &view, Instant::now()).await;
    assert_eq!(farm_working_set(&harness, &worker), None);
    tokio::time::advance(harness.cfg.heartbeat).await;
    worker.step(&ctx, &View::default(), Instant::now()).await;
    assert_eq!(farm_working_set(&harness, &worker), None);
}

/// Catches a reading that outlives the agent's silence: one seen long ago (more than three
/// windows and a poll) is not handed with a later heartbeat. The limit itself is pinned in
/// `load_tests.rs`.
#[tokio::test(start_paused = true)]
async fn a_stale_reading_gives_the_farm_none() {
    let mut harness = Harness::new().await;
    harness.cfg.host_check = Duration::from_secs(1);
    let ctx = harness.ctx();
    assert!(harness.fake.set_host_load("node1", Some(listed(5))));
    let view = polled(&harness).await;

    let (mut late, _messages) = harness.worker().await;
    assert!(late.watch(&ctx, &view, Instant::now()).is_continue());
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(late.beat(&ctx).await.is_continue());
    assert_eq!(farm_working_set(&harness, &late), None);
}
