//! Tests of what a host does around the core's calls (`host`) and of what the adapter tells
//! the invocation recorder (`record`).

use std::sync::Arc;

use bytes::Bytes;
use js_server_core::{Admitted, Beat, Held};
use kbf_front::{Cache, Recorder, Stage, VerifiedBlob};
use kbf_invocation::{AttemptEnd, Outcome as Answered};
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, PackReadCounts, PackWriteCounts};
use kbf_types::{ActionKey, FarmTime, LeaseId, OperationId, WaiterId, WorkerId};
use tokio::sync::watch;
use tonic::Code;

use super::dispatch;
use super::testing::{
    Flaky, adapter, caller, ran, register, request, request_of, rig, rig_caching, rig_over,
    started, starts, submit, submit_recorded, wall,
};

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

/// Asserts that a caller that follows an operation another frontend's daemon runs is shown
/// it executing as soon as it is enrolled, and one that is held here is shown it queued.
#[test]
fn a_caller_following_an_operation_elsewhere_sees_it_executing() {
    let mut adapter = adapter();
    let admitted = |waiter, held| Admitted {
        waiter: WaiterId(waiter),
        operation: OperationId(4),
        joined: true,
        held,
    };
    let (elsewhere, here) = (watch::channel(Stage::Queued), watch::channel(Stage::Queued));

    adapter.enrol(
        &admitted(0, Held::Elsewhere(OperationId(4))),
        caller(1),
        None,
        elsewhere.0,
    );
    adapter.enrol(&admitted(1, Held::Here), caller(1), None, here.0);

    assert_eq!(*elsewhere.1.borrow(), Stage::Executing);
    assert_eq!(*here.1.borrow(), Stage::Queued);
}

/// Asserts that a caller that joins an operation whose `Start` was sent is shown it
/// executing, one that joins an operation still queued is shown it queued, and the mark
/// does not move a stage that has gone on: the operation's answer can come between the
/// core's admission of the request and the mark.
#[tokio::test(start_paused = true)]
async fn a_caller_that_joins_a_started_operation_sees_it_executing() {
    let rig = rig();
    let queued = submit(&rig, 1, 0).expect("queued");
    let joined_queued = submit(&rig, 1, 0).expect("joined");
    assert_ne!(*queued.stage.borrow(), Stage::Executing);
    assert_eq!(
        *joined_queued.stage.borrow(),
        *queued.stage.borrow(),
        "nothing started"
    );
    let mut node = register(&rig, "node-a", 0);
    rig.core.tick(at(0)).await;
    assert!(started(&node.sent()).is_some());
    assert_eq!(*queued.stage.borrow(), Stage::Executing);
    let mut joined = submit(&rig, 1, 0).expect("joined");
    assert_eq!(*joined.stage.borrow_and_update(), Stage::Executing);

    let admitted = Admitted {
        waiter: WaiterId(2),
        operation: OperationId(0),
        joined: true,
        held: Held::Here,
    };
    rig.core
        .with(|view, adapter| adapter.mark_joined(view, &admitted));

    assert!(
        !joined.stage.has_changed().expect("open"),
        "marked again: no wake"
    );
}

/// Asserts that the pack counts of a node's heartbeats are kept for its newest stream: a
/// heartbeat that carries none forgets the node's, and one from a replaced stream is
/// ignored, for reads and for writes alike.
#[tokio::test(start_paused = true)]
async fn the_pack_counts_of_a_node_are_those_of_its_newest_stream() {
    let rig = rig();
    let worker = WorkerId::new("node-a");
    let old = register(&rig, "node-a", 0).stream;
    let new = register(&rig, "node-a", 0).stream;
    let reads = PackReadCounts {
        served: 3,
        ..Default::default()
    };
    let writes = PackWriteCounts {
        packs: 2,
        ..Default::default()
    };
    let drive = |f: &dyn Fn(&mut super::KbfAdapter)| rig.core.with(|_, a| f(a));

    drive(&|a| a.note_pack_reads(&worker, new, Some(reads)));
    drive(&|a| a.note_pack_writes(&worker, new, Some(writes)));
    drive(&|a| a.note_pack_reads(&worker, old, None));
    drive(&|a| a.note_pack_writes(&worker, old, Some(PackWriteCounts::default())));
    let (kept_reads, kept_writes) = rig.core.with(|_, a| (a.pack_reads(), a.pack_writes()));
    assert_eq!(
        kept_reads,
        [(worker.clone(), reads)],
        "the old stream's is ignored"
    );
    assert_eq!(kept_writes, [(worker.clone(), writes)]);

    drive(&|a| a.note_pack_reads(&worker, new, None));
    drive(&|a| a.note_pack_writes(&worker, new, None));
    let (kept_reads, kept_writes) = rig.core.with(|_, a| (a.pack_reads(), a.pack_writes()));
    assert!(kept_reads.is_empty() && kept_writes.is_empty(), "forgotten");
}

/// Asserts that the input cache counts follow the same rules as the pack counts: kept for the
/// newest stream, ignored from a replaced one, forgotten by a heartbeat without them, and shown
/// for the node detail.
#[tokio::test(start_paused = true)]
async fn input_cache_counts_follow_the_newest_stream() {
    use kbf_proto::worker::InputCacheCounts;
    let rig = rig();
    let worker = WorkerId::new("node-a");
    let old = register(&rig, "node-a", 0).stream;
    let new = register(&rig, "node-a", 0).stream;
    let counts = InputCacheCounts {
        fills: 4,
        ..Default::default()
    };
    let drive = |f: &dyn Fn(&mut super::KbfAdapter)| rig.core.with(|_, a| f(a));
    drive(&|a| a.note_input_cache(&worker, new, Some(counts)));
    drive(&|a| a.note_input_cache(&worker, old, None));
    let kept = rig
        .core
        .with(|_, a| (a.input_caches(), a.input_cache_of(&worker)));
    assert_eq!(
        kept.0,
        [(worker.clone(), counts)],
        "the old stream's is ignored"
    );
    assert_eq!(kept.1.map(|view| view.fills), Some(4));
    drive(&|a| a.note_input_cache(&worker, new, None));
    let kept = rig
        .core
        .with(|_, a| (a.input_caches(), a.input_cache_of(&worker)));
    assert!(kept.0.is_empty() && kept.1.is_none(), "forgotten");
}

/// Asserts that a node whose newest stream has closed is no longer connected in the
/// machine's registry once the adapter is told to look, and one whose stream is open still
/// is.
#[tokio::test(start_paused = true)]
async fn a_node_whose_stream_closed_is_not_connected_once_the_adapter_looks() {
    let rig = rig();
    let open = register(&rig, "node-a", 0);
    let closed = register(&rig, "node-b", 0);
    drop(closed.inbox);

    let connected = rig.core.with(|_, a| {
        a.note_closed_streams();
        let node = |name: &str| a.machine().node(&WorkerId::new(name)).map(|n| n.connected);
        (node("node-a"), node("node-b"))
    });

    assert_eq!(connected, (Some(true), Some(false)));
    drop(open);
}

/// The `Result` of a run of `lease` that passed its own memory limit and was killed.
fn killed_at_its_limit(lease: LeaseId) -> worker::Result {
    worker::Result {
        status: Some(rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "killed".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: worker::MemoryKill::OwnLimit as i32,
        action_result: None,
        ..ran(lease, 0)
    }
}

/// Asserts that an operation no recorded call waits on costs the recorder nothing: its
/// grants, starts, the lease given up when its run is killed, its finish and the refusal of
/// another are all told to nobody. Each seam is a set lookup.
#[tokio::test(start_paused = true)]
async fn an_operation_no_recorded_call_waits_on_is_not_told_to_the_recorder() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let mut node = register(&rig, "node-a", 0);
    submit(&rig, 1, 0).expect("queued");
    submit_unservable(&rig);
    let first = started(&node.sent()).expect("started");
    assert!(report(&rig, killed_at_its_limit(first)).await);
    rig.core.tick(at(0)).await;
    let second = started(&node.sent()).expect("started again");
    assert!(report(&rig, ran(second, 0)).await);

    beat_past_the_unservable_wait(&rig, &node).await;

    recorder.caught_up().await;
    assert_eq!(recorder.sent(), 0, "nothing was told");
    rig.core
        .with(|_, a| a.recording.ran(OperationId(0), first, Default::default()));
    recorder.caught_up().await;
    assert_eq!(
        recorder.sent(),
        1,
        "a measurement is sent as the host hands it over"
    );
}

/// Submits an action no node can run.
fn submit_unservable(rig: &super::testing::Rig) {
    let mut unservable = super::testing::submission(2);
    unservable.request = request_of(2, 1, 64);
    let shapes = Default::default();
    dispatch::submit(&rig.core, at(0), &shapes, unservable).expect("queued");
}

/// Heartbeats `node` through the wait after which the core refuses work no node can run.
async fn beat_past_the_unservable_wait(rig: &super::testing::Rig, node: &super::testing::Wire) {
    let worker = WorkerId::new("node-a");
    for (seq, t) in [
        (1, 50),
        (2, 100),
        (3, 150),
        (4, 200),
        (5, 250),
        (6, 299),
        (7, 301),
    ] {
        let beat = Beat::new(seq, Vec::new());
        let now = at(t * 1_000);
        let beat = rig.core.heartbeat(now, &worker, node.stream, beat).await;
        beat.expect("heard");
        rig.core.tick(now).await;
    }
}

/// Asserts what the invocation recorder is told of a recorded call over a core: its answer,
/// each lease granted and started, the lease given up (a run killed at its own memory
/// limit), and the end of the operation with the lease that accepted it. The record of the
/// call shows two attempts, the first requeued and the second accepted, and the callers'
/// answer.
#[tokio::test(start_paused = true)]
async fn a_recorded_call_is_told_each_lease_and_the_end_of_its_operation() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let mut node = register(&rig, "node-a", 0);
    let ticket = submit_recorded(&rig, &recorder, request(1), "build-1").expect("queued");
    let first = started(&node.sent()).expect("started");
    let ack = report(&rig, killed_at_its_limit(first)).await;
    assert!(ack, "the kill is taken");
    rig.core.tick(at(0)).await;
    let second = started(&node.sent()).expect("started again");
    assert_ne!(first, second);

    assert!(report(&rig, ran(second, 0)).await);

    recorder.caught_up().await;
    let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new("build-1"));
    let action = ActionKey::try_from(&request(1).key)
        .expect("an action")
        .action;
    let record = recorder.get(&key).expect("a record");
    let row = record.row(&action).expect("a row");
    let ends: Vec<_> = row.attempts.iter().map(|a| a.end.clone()).collect();
    assert!(
        matches!(
            &ends[..],
            [Some(AttemptEnd::Requeued(_)), Some(AttemptEnd::Accepted)]
        ),
        "{ends:?}"
    );
    assert!(
        row.attempts.iter().all(|a| a.started.is_some()),
        "{:?}",
        row.attempts
    );
    assert_eq!(row.outcome, Some(Answered::Ok { exit_code: 0 }));
    assert_eq!(row.operation.as_deref(), Some(ticket.name.as_str()));
}

/// Asserts that a recorded call whose operation the core refuses (no live node can run it
/// for the unservable wait) ends with the reason, and that the measurement of a run is sent
/// to the recorder as the host hands it over.
#[tokio::test(start_paused = true)]
async fn a_recorded_call_whose_operation_is_refused_ends_with_the_reason() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let node = register(&rig, "node-a", 0);
    submit_recorded(&rig, &recorder, request_of(1, 1, 64), "build-2").expect("queued");

    beat_past_the_unservable_wait(&rig, &node).await;

    recorder.caught_up().await;
    let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new("build-2"));
    let action = ActionKey::try_from(&request_of(1, 1, 64).key)
        .expect("an action")
        .action;
    let record = recorder.get(&key).expect("a record");
    let outcome = record.row(&action).expect("a row").outcome.clone();
    assert!(matches!(outcome, Some(Answered::Refused(_))), "{outcome:?}");
    rig.core.with(|_, a| {
        assert!(
            !a.recording.is_recorded(OperationId(0)),
            "the operation is not noted any more"
        );
        a.recording
            .ran(OperationId(0), LeaseId::new(7, 0), Default::default());
    });
}

/// Asserts what the recorder is told of an operation that ends otherwise than by an OK
/// result: one the daemon found invalid ends as a failure with its code and message and the
/// daemon's reason on the attempt; one that timed out ends as a timeout, with the listing of
/// the log it wrote before when the cache holds it, and without when the daemon reported none.
#[tokio::test(start_paused = true)]
async fn the_recorder_is_told_how_failed_and_timed_out_operations_ended() {
    let cache = Arc::new(Cache::memory());
    let log = VerifiedBlob::hashed(Bytes::from_static(b"what it wrote before the timeout"));
    let digest = kbf_front::digest_to_proto(&log.digest());
    cache.store_blobs(vec![log]).await.expect("stored");
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_caching(&Flaky::new(), recorder.clone(), cache.into());
    let mut node = register(&rig, "node-a", 0);
    for (tag, call) in [(1, "invalid"), (2, "slow"), (3, "silent")] {
        submit_recorded(&rig, &recorder, request(tag), call).expect("queued");
    }
    let leases = starts(&node.sent());
    assert_eq!(leases.len(), 3);
    let status = |code: Code, message: &str| {
        Some(rpc::Status {
            code: code as i32,
            message: message.to_owned(),
            details: Vec::new(),
        })
    };
    let ended = |lease, status, action_result| worker::Result {
        status,
        action_result,
        ..ran(lease, 0)
    };
    let written = ActionResult {
        stdout_digest: Some(digest),
        ..Default::default()
    };

    report(
        &rig,
        ended(leases[0], status(Code::InvalidArgument, "bad input"), None),
    )
    .await;
    report(
        &rig,
        ended(
            leases[1],
            status(Code::DeadlineExceeded, "slow"),
            Some(written),
        ),
    )
    .await;
    report(
        &rig,
        ended(leases[2], status(Code::DeadlineExceeded, "silent"), None),
    )
    .await;

    recorder.caught_up().await;
    let row = |tag: u8, call: &str| {
        let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new(call));
        let action = ActionKey::try_from(&request(tag).key)
            .expect("an action")
            .action;
        let record = recorder.get(&key).expect("a record");
        record.row(&action).expect("a row").clone()
    };
    let invalid = row(1, "invalid");
    let failed = Answered::Failed {
        code: Code::InvalidArgument as i32,
        message: "bad input".to_owned(),
    };
    assert_eq!(invalid.outcome, Some(failed));
    let end = invalid.attempts[0].end.clone();
    assert!(
        matches!(&end, Some(AttemptEnd::Failed(_, why)) if why == "bad input"),
        "{end:?}"
    );
    let (slow, silent) = (row(2, "slow"), row(3, "silent"));
    assert_eq!(slow.outcome, Some(Answered::TimedOut));
    assert!(slow.listing.is_some(), "the log it wrote is listed");
    assert_eq!(silent.outcome, Some(Answered::TimedOut));
    assert!(silent.listing.is_none());
}

/// The report of `result` from node-a, and whether the core acknowledged it as accepted.
async fn report(rig: &super::testing::Rig, result: worker::Result) -> bool {
    let ack = dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        &|| at(0),
        &WorkerId::new("node-a"),
        result,
    )
    .await
    .expect("heard");
    ack.is_some_and(|ack| ack.accepted)
}

/// Asserts that the farm's number of a stream is the core's, that a stream is found from it
/// only while it is the worker's newest, and that a status on a replaced stream or of a worker
/// the core never heard is not kept, while one on the newest stream is. Catches: a replaced stream found by its number, which
/// lets a restarted daemon's old stream overwrite its status.
#[tokio::test(start_paused = true)]
async fn a_stream_is_found_by_its_number_while_it_is_the_workers_newest() {
    let rig = rig();
    let old = register(&rig, "node-a", 0);
    let new = register(&rig, "node-a", 0);
    let (old_number, new_number) = (
        super::host::stream_number(old.stream).expect("numbered"),
        super::host::stream_number(new.stream).expect("numbered"),
    );
    let node = WorkerId::new("node-a");
    let ghost = WorkerId::new("ghost");

    rig.core.with(|_, a| {
        assert_ne!(old_number, new_number);
        assert_eq!(a.current_stream(&node, new_number), Some(new.stream));
        assert_eq!(a.current_stream(&node, old_number), None);
        assert_eq!(a.current_stream(&ghost, new_number), None);
        let status = || kbf_proto::update::UpdateStatus::default();
        assert!(!a.report_update_status(&node, old.stream, status()));
        assert!(a.report_update_status(&node, new.stream, status()));
        assert!(
            a.report_node_status(&node, old.stream, Default::default())
                .is_empty()
        );
        assert!(a.machine().node(&node).is_some_and(|n| n.update_live));
        assert!(a.machine().node(&node).is_some_and(|n| n.status.is_none()));
        a.report_node_status(&node, new.stream, Default::default());
        assert!(
            a.machine().node(&node).is_some_and(|n| n.status.is_some()),
            "a status on the newest stream is kept"
        );
    });
}
