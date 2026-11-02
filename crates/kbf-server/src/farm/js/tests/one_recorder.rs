//! What the invocation recorder is told, driven through a legacy farm and a js farm with the
//! same scripted input: both must leave the same run history, the same counters and the
//! same export rows (the run records the history writer is given).

use kbf_front::{CallMetadata, ExecuteCall, Recorder};
use kbf_history::records::RunRecord;
use kbf_invocation::{AttemptEnd, How, InvocationId, Outcome, RecordKey, Run};
use kbf_proto::worker::MemoryKill;
use kbf_types::{ActionKey, Digest, WorkerId};

use super::hot::{
    backend, farm_on, join, killed, linux, node, ok, request, rerun_start, submission, usage,
};
use crate::SchedulerCore;

/// One attempt as the history shows it, with every wall time reduced to whether it was
/// set (the two farms run at different instants).
#[derive(Debug, PartialEq)]
struct AttemptSeen {
    worker: WorkerId,
    end: Option<AttemptEnd>,
    started: bool,
    ended: bool,
    run: Option<Run>,
}

/// One action row as the history shows it.
#[derive(Debug, PartialEq)]
struct RowSeen {
    action: Digest,
    how: How,
    outcome: Option<Outcome>,
    listed: bool,
    attempts: Vec<AttemptSeen>,
}

/// What an invocation's aggregates came to, but for the times.
#[derive(Debug, PartialEq)]
struct Counters {
    reruns: Vec<(&'static str, u64)>,
    usage_actions: u64,
    cpu_micros_sum: u64,
    cpu_micros_max: u64,
    peak_memory_sum: u64,
    peak_memory_max: u64,
}

/// Everything the recorder holds after the script, by invocation.
#[derive(Debug, PartialEq)]
struct Seen {
    sent: u64,
    dropped: u64,
    history: Vec<(&'static str, Vec<RowSeen>)>,
    counters: Vec<(&'static str, Counters)>,
    exported: Vec<(&'static str, Vec<RunRecord>)>,
}

const INVOCATIONS: [&str; 2] = ["build-1", "build-2"];

fn set(micros: u64) -> u64 {
    u64::from(micros != 0)
}

/// An Execute call of invocation `invocation` for the action of `request(tag)`.
fn call(recorder: &Recorder, invocation: &str, tag: u8) -> ExecuteCall {
    let metadata = CallMetadata {
        invocation_id: kbf_front::InvocationId::parse(invocation),
        ..Default::default()
    };
    let action = ActionKey::try_from(&request(tag).key)
        .expect("an action")
        .action;
    ExecuteCall::new(
        recorder,
        &metadata,
        &Default::default(),
        "main",
        action,
        action,
    )
    .expect("a recorded call")
}

/// Asserts that every attempt of `record` was queued, then granted, started and ended, in
/// that order wherever the time is set, and that an action's attempts follow one another.
/// The queue wait (start minus queued) and the wall (end minus start) are read from these.
fn assert_times_in_order(record: &kbf_invocation::Invocation, core: SchedulerCore) {
    for row in record.rows() {
        let mut previous_end = 0;
        for a in &row.attempts {
            let times = [Some(a.queued), Some(a.granted), a.started, a.ended];
            let mut last = previous_end;
            for t in times.into_iter().flatten() {
                let t = t.unix_micros();
                assert!(t >= last, "{core:?}: {a:?} is out of order");
                last = t;
            }
            previous_end = a.ended.map_or(previous_end, |e| e.unix_micros());
        }
    }
}

/// The script, over a farm on `core`: one action runs to its result while a second call
/// joins it, another is killed under node pressure, given up, run again and finished.
async fn scenario(core: SchedulerCore) -> Seen {
    use kbf_front::Dispatch as _;
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    let farm = farm_on(core, &backend()).recording(recorder.clone());
    let mut joined = join(&farm, "n1", linux());

    let first = call(&recorder, "build-1", 1);
    farm.submit(submission(1, Some(first))).expect("queued");
    let lease = joined.started().expect("a Start");
    let twin = call(&recorder, "build-2", 1);
    farm.submit(submission(1, Some(twin))).expect("joined");
    let ack = farm
        .report(&node("n1"), ok(&lease, Some(&usage())))
        .await
        .expect("an ack");
    assert!(ack.accepted, "{ack:?}");

    let second = call(&recorder, "build-1", 2);
    farm.submit(submission(2, Some(second))).expect("queued");
    let lease = joined.started().expect("a Start");
    let ack = farm
        .report(&node("n1"), killed(&lease, MemoryKill::NodePressure))
        .await
        .expect("an ack");
    assert!(ack.accepted, "{ack:?}");
    let lease = rerun_start(&farm, &mut joined).await;
    let ack = farm
        .report(&node("n1"), ok(&lease, Some(&usage())))
        .await
        .expect("an ack");
    assert!(ack.accepted, "{ack:?}");

    recorder.caught_up().await;
    let mut seen = Seen {
        sent: recorder.sent(),
        dropped: recorder.dropped(),
        history: Vec::new(),
        counters: Vec::new(),
        exported: Vec::new(),
    };
    for name in INVOCATIONS {
        let record = recorder
            .get(&RecordKey::owner(InvocationId::new(name)))
            .expect("a record");
        let rows = record
            .rows()
            .iter()
            .map(|row| RowSeen {
                action: row.action,
                how: row.how,
                outcome: row.outcome.clone(),
                listed: row.listing.is_some(),
                attempts: row
                    .attempts
                    .iter()
                    .map(|a| AttemptSeen {
                        worker: a.worker.clone(),
                        end: a.end.clone(),
                        started: a.started.is_some(),
                        ended: a.ended.is_some(),
                        run: a.run.as_deref().map(|run| Run {
                            times: Default::default(),
                            ..run.clone()
                        }),
                    })
                    .collect(),
            })
            .collect();
        seen.history.push((name, rows));
        assert_times_in_order(&record, core);
        let m = record.metrics();
        seen.counters.push((
            name,
            Counters {
                reruns: m.reruns.into_iter().collect(),
                usage_actions: m.usage_actions,
                cpu_micros_sum: m.cpu_micros_sum,
                cpu_micros_max: m.cpu_micros_max,
                peak_memory_sum: m.peak_memory_sum,
                peak_memory_max: m.peak_memory_max,
            },
        ));
        let runs = record
            .rows()
            .iter()
            .flat_map(|row| {
                row.attempts
                    .iter()
                    .map(|a| crate::history::run_record(row, a))
            })
            .map(|mut run| {
                run.attempt_queued_us = set(run.attempt_queued_us);
                run.started_us = set(run.started_us);
                run.ended_us = set(run.ended_us);
                run.lease_epoch = 0;
                run.lease_seq = 0;
                if let Some(t) = run.timestamps.as_mut() {
                    for field in [
                        &mut t.queued_us,
                        &mut t.granted_us,
                        &mut t.worker_start_us,
                        &mut t.input_fetch_start_us,
                        &mut t.input_fetch_completed_us,
                        &mut t.execution_start_us,
                        &mut t.execution_completed_us,
                        &mut t.output_upload_start_us,
                        &mut t.output_upload_completed_us,
                        &mut t.worker_completed_us,
                    ] {
                        *field = set(*field);
                    }
                }
                // The stored result carries the times the server stamped on it, so both its hash
                // and its size vary with the wall clock (a varint's length follows its value):
                // only whether a result was stored is compared.
                if let Some(result) = run.result.as_mut() {
                    result.hash.clear();
                    result.size = 0;
                }
                run
            })
            .collect();
        seen.exported.push((name, runs));
    }
    seen
}

/// Asserts that a legacy farm and a js farm, given the same scripted input (a call that
/// runs, one that joins it, an action killed under node pressure and run again), tell the
/// recorder the same number of events and leave the same history rows, the same counters
/// and the same export rows. Catches: a seam of one core that tells the recorder less or
/// more than the other's, such as a lease whose start, kill or end one core never sends.
#[tokio::test(start_paused = true)]
async fn both_cores_record_the_same_history_counters_and_export_rows() {
    let legacy = scenario(SchedulerCore::Legacy).await;
    let js = scenario(SchedulerCore::Js).await;
    assert_eq!(js, legacy);
}

/// Asserts that the script leaves a history worth comparing: the joined call's row is the
/// twin's, the killed action has two attempts of which the first was given up, and the
/// aggregates and export rows are not empty. Catches: an equality test that holds because
/// both sides recorded nothing.
#[tokio::test(start_paused = true)]
async fn the_script_leaves_attempts_counters_and_export_rows_to_compare() {
    for core in [SchedulerCore::Legacy, SchedulerCore::Js] {
        let seen = scenario(core).await;
        let rows = |name| {
            &seen
                .history
                .iter()
                .find(|(n, _)| *n == name)
                .expect("kept")
                .1
        };
        let first = rows("build-1");
        assert_eq!(first.len(), 2, "{core:?}");
        assert_eq!(first[0].attempts.len(), 1, "{core:?}");
        assert!(first[0].attempts[0].started, "{core:?}");
        assert_eq!(first[1].attempts.len(), 2, "{core:?}");
        assert!(
            matches!(first[1].attempts[0].end, Some(AttemptEnd::Requeued(_))),
            "{core:?}"
        );
        let twin = rows("build-2");
        assert_eq!(twin.len(), 1, "{core:?}");
        assert_eq!(twin[0].how, How::Joined, "{core:?}");
        let counters = &seen.counters[0].1;
        assert!(!counters.reruns.is_empty(), "{core:?}");
        assert_eq!(seen.exported[0].1.len(), 3, "{core:?}");
    }
}
