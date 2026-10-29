//! Tests of the work the adapter does around a `Result` the core feeds (`report_hooks`): the
//! `action usage:` line, the invocation record's `Ran` event with the booking of a memory
//! kill, the node memory-pressure line, and the end of the worker streams.

use kbf_front::Recorder;
use kbf_invocation::{InvocationId, RecordKey};
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{self, CgroupUsage, MemoryKill, ResourceUsage};
use kbf_types::{ActionKey, FarmTime, LeaseId, WorkerId};
use tonic::{Code, Status};

use super::dispatch;
use super::testing::{Flaky, register, request, result_with, rig, rig_over, started, submit, wall};
use crate::test_log::capture;

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

/// `lease`'s `Result` that exited 0, carrying `usage` the way a daemon sends it.
fn measured(lease: LeaseId) -> worker::Result {
    let usage = ResourceUsage {
        wall_micros: 2_000_000,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(3_000_000),
            cpu_user_micros: Some(2_000_000),
            cpu_system_micros: Some(1_000_000),
            cpu_periods: Some(0),
            cpu_throttled_periods: Some(0),
            cpu_throttled_micros: Some(0),
            memory_peak_bytes: Some(4096),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    };
    result_with(
        lease,
        ActionResult {
            exit_code: 0,
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage.to_any()],
                ..Default::default()
            }),
            ..Default::default()
        },
    )
}

/// `lease`'s `Result` killed for memory as `kill` says, with the status a driver sends.
fn killed(lease: LeaseId, kill: MemoryKill) -> worker::Result {
    worker::Result {
        status: Some(rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: kill as i32,
        ..result_with(lease, ActionResult::default())
    }
}

async fn report(
    rig: &super::testing::Rig,
    worker: &str,
    result: worker::Result,
) -> worker::ResultAck {
    dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        || at(0),
        &WorkerId::new(worker),
        result,
    )
    .await
    .expect("a host the core hears")
    .expect("an ack")
}

/// Asserts a report the scheduler took that measured what the lease used logs one `action
/// usage:` line naming the node and how the lease ended, and a copy of it, which the core
/// refuses, logs none. Catches: the measurement lost between the host and the hook, a line for
/// a refused copy, and two lines for one report.
#[tokio::test(start_paused = true)]
async fn a_taken_report_logs_one_action_usage_line_and_its_copy_none() {
    let log = capture();
    let rig = rig();
    let mut node = register(&rig, "usage-node", 0);
    let _ticket = submit(&rig, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");

    assert!(report(&rig, "usage-node", measured(lease)).await.accepted);
    assert!(!report(&rig, "usage-node", measured(lease)).await.accepted);

    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("action usage:"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    for part in [
        "exit_code=0",
        "wall_us=2000000",
        "outcome=ok",
        "node=usage-node",
    ] {
        assert!(lines[0].contains(part), "{part}: {}", lines[0]);
    }
}

/// Asserts a run killed for memory is told to the invocation record with the booking of the
/// kill (the memory booked, and the cap of a kill at its own limit), and a run that ended
/// otherwise with none. Catches: the `Ran` event never sent from the hook, or sent without the
/// kill's booking, or a booking taken from a kill of another worker.
#[tokio::test(start_paused = true)]
async fn a_run_killed_for_memory_is_recorded_with_its_booking() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let mut node = register(&rig, "oom-node", 0);
    let _ticket =
        super::testing::submit_recorded(&rig, &recorder, request(1), "build-1").expect("queued");
    let lease = started(&node.sent()).expect("started");

    assert!(
        report(&rig, "oom-node", killed(lease, MemoryKill::OwnLimit))
            .await
            .accepted
    );

    recorder.caught_up().await;
    let action = ActionKey::try_from(&request(1).key)
        .expect("an action")
        .action;
    let record = recorder
        .get(&RecordKey::owner(InvocationId::new("build-1")))
        .expect("a record");
    let run = record.row(&action).expect("a row").attempts[0]
        .run
        .clone()
        .expect("the killed run");
    assert!(run.booked_memory_bytes.is_some(), "{run:?}");
    assert!(run.cap_bytes.is_some(), "{run:?}");
}

/// Asserts a run of a recorded operation that ended with a result the scheduler took is
/// recorded without a booking, and a kill by the node's memory pressure is logged for
/// operators with the node's count. Catches: a booking on a run no kill ended, and no
/// pressure line.
#[tokio::test(start_paused = true)]
async fn a_run_that_was_not_killed_has_no_booking_and_a_pressure_kill_is_logged() {
    let log = capture();
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let rig = rig_over(&Flaky::new(), recorder.clone());
    let mut node = register(&rig, "busy-node", 0);
    let _ticket =
        super::testing::submit_recorded(&rig, &recorder, request(2), "build-2").expect("queued");
    let lease = started(&node.sent()).expect("started");

    assert!(
        report(&rig, "busy-node", killed(lease, MemoryKill::NodePressure))
            .await
            .accepted
    );
    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("kills=") && l.contains("node=busy-node"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("kills=1"), "{}", lines[0]);

    // The rerun succeeds: its run has no booking.
    rig.core.tick(at(1)).await;
    let rerun = started(&node.sent()).expect("the rerun");
    assert!(report(&rig, "busy-node", measured(rerun)).await.accepted);
    recorder.caught_up().await;
    let action = ActionKey::try_from(&request(2).key)
        .expect("an action")
        .action;
    let record = recorder
        .get(&RecordKey::owner(InvocationId::new("build-2")))
        .expect("a record");
    let attempts = &record.row(&action).expect("a row").attempts;
    let last = attempts.last().and_then(|a| a.run.clone()).expect("a run");
    assert_eq!(last.booked_memory_bytes, None);
}

/// Asserts ending the worker streams sends each open stream the status and counts only the
/// streams still open. Catches: a count of closed streams, a stream never told.
#[tokio::test(start_paused = true)]
async fn ending_the_streams_tells_the_open_ones_and_counts_them() {
    let rig = rig();
    let mut open = register(&rig, "open", 0);
    let closed = register(&rig, "closed", 0);
    drop(closed.inbox);

    let ended = rig
        .core
        .with(|_, adapter| adapter.end_streams(&Status::unavailable("going away")));

    assert_eq!(ended, 1);
    let told = std::iter::from_fn(|| open.inbox.try_recv().ok()).find_map(Result::err);
    assert_eq!(told.map(|status| status.code()), Some(Code::Unavailable));
}
