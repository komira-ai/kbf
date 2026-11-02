//! Tests of [`Recording`] by itself: what it tells the invocation recorder of an operation a
//! recorded call waits on, and that it tells nothing of any other. Both scheduler cores hold
//! this one type, so these tests cover each of them.

use kbf_front::{CallMetadata, ExecuteCall, Finished, Recorder};
use kbf_invocation::{
    AttemptEnd, How, Invocation, InvocationId, Outcome as Answered, RecordKey, Run,
};
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_sched::{Requeue, RequeueReason};
use kbf_types::{
    ActionKey, Answer, Digest, DigestFunction, Failure, FencePolicy, LeaseGrant, LeaseId,
    LeaseKind, OperationId, Outcome, Refusal, Resources, ResultRef, StartLease, WorkerId,
};
use tonic::Code;

use super::Recording;

fn action(tag: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

fn lease(seq: u64) -> LeaseId {
    LeaseId::new(7, seq)
}

/// A recording telling a recorder that is on, and that recorder.
fn rig() -> (Recording, Recorder) {
    let (recorder, task) = Recorder::new(1 << 20, 64);
    tokio::spawn(task.run());
    let mut recording = Recording::new();
    recording.tell(recorder.clone());
    (recording, recorder)
}

/// An Execute call of invocation `invocation` for `action(tag)`.
fn call(recorder: &Recorder, invocation: &str, tag: u8) -> ExecuteCall {
    let metadata = CallMetadata {
        invocation_id: kbf_front::InvocationId::parse(invocation),
        ..Default::default()
    };
    ExecuteCall::new(
        recorder,
        &metadata,
        &Default::default(),
        "main",
        action(tag),
        action(tag),
    )
    .expect("a recorded call")
}

fn grant(operation: u64, seq: u64) -> LeaseGrant {
    LeaseGrant {
        lease: lease(seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-a"),
        cost: Default::default(),
    }
}

fn start(operation: u64, seq: u64) -> StartLease {
    let key = ActionKey {
        instance: "main".to_owned(),
        action: action(1),
    };
    StartLease {
        worker: WorkerId::new("node-a"),
        lease: lease(seq),
        operation: OperationId(operation),
        key: key.into(),
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: FencePolicy::RunOn,
        device: None,
    }
}

fn requeue(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: lease(seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::Silent,
    }
}

fn answer(operation: u64, seq: u64, outcome: Outcome) -> Answer {
    Answer {
        operation: OperationId(operation),
        lease: lease(seq),
        waiters: Vec::new(),
        outcome,
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

fn completed() -> Outcome {
    Outcome::Completed {
        action_result: ResultRef::new([9; 32], 9),
    }
}

fn ran_ok() -> Finished {
    Finished::Ran(Box::<ActionResult>::default())
}

fn status(code: Code) -> rpc::Status {
    rpc::Status {
        code: code as i32,
        message: "why".to_owned(),
        details: Vec::new(),
    }
}

/// The record of `invocation` once the recorder has applied everything sent.
async fn record(recorder: &Recorder, invocation: &str) -> Invocation {
    recorder.caught_up().await;
    recorder
        .get(&RecordKey::owner(InvocationId::new(invocation)))
        .expect("a record")
}

/// Asserts that no event of any kind is sent for an operation no recorded call waits on:
/// a grant, a start, a lease given up, an end the scheduler made, an end another frontend
/// made, and a refusal. Catches: a seam that skips the set lookup and tells the recorder of
/// every operation.
#[tokio::test(start_paused = true)]
async fn an_unrecorded_operation_emits_nothing_for_any_event() {
    let (mut recording, recorder) = rig();
    assert!(!recording.is_recorded(OperationId(5)));
    recording.granted(&grant(5, 1));
    recording.started(&start(5, 1));
    recording.requeued(&requeue(5, 1));
    recording.finished(&answer(5, 2, completed()), &ran_ok(), None, None);
    recording.followed(OperationId(5), &ran_ok());
    recording.refused(&Refusal {
        operation: OperationId(5),
        waiters: Vec::new(),
        reason: "no node".to_owned(),
    });
    recorder.caught_up().await;
    assert_eq!(recorder.sent(), 0);
}

/// Asserts that a measurement is sent as given: the caller, which asks `is_recorded` before
/// it builds one, is the gate. Catches: a gate here that the hosts' tests of a measurement
/// without a call would not reach, or a measurement dropped.
#[tokio::test(start_paused = true)]
async fn a_measurement_is_sent_as_given_because_its_callers_ask_first() {
    let (recording, recorder) = rig();
    recording.ran(OperationId(5), lease(1), Run::default());
    recorder.caught_up().await;
    assert_eq!(recorder.sent(), 1);
}

/// Asserts that a recorded operation's events are all sent, in the order the scheduler made
/// the changes: the call's answer, two leases each granted and started, the first given up,
/// and the end with the second lease. Catches: an event never sent, one sent out of order
/// (the attempts would be in another order, or the end would name the wrong lease), and an
/// operation still noted after its end.
#[tokio::test(start_paused = true)]
async fn a_recorded_operation_emits_each_event_in_order() {
    let (mut recording, recorder) = rig();
    let op = OperationId(1);
    recording.submit(call(&recorder, "build-1", 1), "operations/1", op, false);
    assert!(recording.is_recorded(op));
    recording.granted(&grant(1, 1));
    recording.started(&start(1, 1));
    recording.requeued(&requeue(1, 1));
    recording.granted(&grant(1, 2));
    recording.started(&start(1, 2));
    recording.finished(&answer(1, 2, completed()), &ran_ok(), None, None);
    assert!(!recording.is_recorded(op), "noted until its end only");
    let record = record(&recorder, "build-1").await;
    assert_eq!(recorder.sent(), 8, "two for the call, then six");
    let row = record.row(&action(1)).expect("a row");
    assert_eq!(row.how, How::Ran);
    assert_eq!(row.outcome, Some(Answered::Ok { exit_code: 0 }));
    let leases: Vec<LeaseId> = row.attempts.iter().map(|a| a.lease).collect();
    assert_eq!(leases, [lease(1), lease(2)]);
    assert_eq!(
        row.attempts[0].end,
        Some(AttemptEnd::Requeued(RequeueReason::Silent))
    );
    assert_eq!(row.attempts[1].end, Some(AttemptEnd::Accepted));
    assert!(row.attempts.iter().all(|a| a.started.is_some()));
}

/// Asserts that a call that joins an operation already in flight is recorded on the twin's
/// operation: both calls' rows hold the one operation's attempts and its end, the operation
/// is noted once, and its end forgets it for both. Catches: a joined call that notes nothing
/// (its row would never end), or one that notes the operation again so the first end leaves
/// it noted.
#[tokio::test(start_paused = true)]
async fn a_joined_call_is_recorded_on_the_twin() {
    let (mut recording, recorder) = rig();
    let op = OperationId(1);
    recording.submit(call(&recorder, "build-1", 1), "operations/1", op, false);
    recording.submit(call(&recorder, "build-2", 1), "operations/1", op, true);
    recording.granted(&grant(1, 1));
    recording.started(&start(1, 1));
    recording.finished(&answer(1, 1, completed()), &ran_ok(), None, None);
    assert!(!recording.is_recorded(op));
    let twin = record(&recorder, "build-2").await;
    let row = twin.row(&action(1)).expect("a row");
    assert_eq!(row.how, How::Joined);
    assert_eq!(row.attempts.len(), 1);
    assert_eq!(row.outcome, Some(Answered::Ok { exit_code: 0 }));
    let owner = record(&recorder, "build-1").await;
    let row = owner.row(&action(1)).expect("a row");
    assert_eq!(row.how, How::Ran);
    assert_eq!(row.attempts.len(), 1);
}

/// Asserts that an operation ends once: an end after the end sends nothing, whichever way
/// it comes (the scheduler's, another frontend's, a refusal). Catches: an end that does not
/// forget the operation, so a late duplicate would end the row again.
#[tokio::test(start_paused = true)]
async fn an_operation_that_has_ended_emits_nothing_more() {
    let (mut recording, recorder) = rig();
    let op = OperationId(1);
    recording.submit(call(&recorder, "build-1", 1), "operations/1", op, false);
    recording.finished(&answer(1, 1, completed()), &ran_ok(), None, None);
    recorder.caught_up().await;
    let told = recorder.sent();
    recording.finished(&answer(1, 1, completed()), &ran_ok(), None, None);
    recording.followed(op, &ran_ok());
    recording.refused(&Refusal {
        operation: op,
        waiters: Vec::new(),
        reason: "late".to_owned(),
    });
    recorder.caught_up().await;
    assert_eq!(recorder.sent(), told);
}

/// Asserts how each kind of end is answered in the row: an OK result with its exit code, a
/// result that timed out, a failed status that is DEADLINE_EXCEEDED (a timeout), and any other
/// failed status with its code and message. Catches: a status mapped to the wrong answer, or
/// a code and message lost.
#[tokio::test(start_paused = true)]
async fn an_end_is_answered_ok_timed_out_or_failed_as_the_callers_were_told() {
    let (mut recording, recorder) = rig();
    let cases = [
        (1_u8, ran_ok(), Answered::Ok { exit_code: 0 }),
        (
            2,
            Finished::TimedOut {
                status: status(Code::DeadlineExceeded),
                partial: Box::default(),
            },
            Answered::TimedOut,
        ),
        (
            3,
            Finished::Failed(status(Code::DeadlineExceeded)),
            Answered::TimedOut,
        ),
        (
            4,
            Finished::Failed(status(Code::Internal)),
            Answered::Failed {
                code: Code::Internal as i32,
                message: "why".to_owned(),
            },
        ),
    ];
    for (tag, finished, _) in &cases {
        let op = OperationId(u64::from(*tag));
        let invocation = format!("build-{tag}");
        recording.submit(
            call(&recorder, &invocation, *tag),
            "operations/x",
            op,
            false,
        );
        recording.finished(
            &answer(u64::from(*tag), 1, completed()),
            finished,
            None,
            None,
        );
    }
    for (tag, _, expected) in cases {
        let record = record(&recorder, &format!("build-{tag}")).await;
        let row = record.row(&action(tag)).expect("a row");
        assert_eq!(row.outcome.as_ref(), Some(&expected), "case {tag}");
    }
}

/// Asserts what an end carries beyond its answer: the daemon's reason on the attempt of a
/// failure (and nothing when the lease reported none), the listing of a stored result, the
/// listing of a timed-out run's partial result, and no listing for a failure. Catches: a
/// reason dropped or invented, a listing built for an end that has none, or a stored result
/// left out of its row.
#[tokio::test(start_paused = true)]
async fn an_end_carries_the_daemons_reason_and_the_listing_of_what_it_stored() {
    let (mut recording, recorder) = rig();
    let failed = || answer(1, 1, Outcome::Failed(Failure::Invalid));
    recording.submit(
        call(&recorder, "build-1", 1),
        "operations/1",
        OperationId(1),
        false,
    );
    recording.granted(&grant(1, 1));
    recording.finished(
        &failed(),
        &Finished::Failed(status(Code::InvalidArgument)),
        None,
        Some("bad image".to_owned()),
    );
    recording.submit(
        call(&recorder, "build-2", 2),
        "operations/2",
        OperationId(2),
        false,
    );
    recording.granted(&grant(2, 1));
    recording.finished(
        &answer(2, 1, Outcome::Failed(Failure::Infra)),
        &Finished::Failed(status(Code::Internal)),
        None,
        None,
    );
    recording.submit(
        call(&recorder, "build-3", 3),
        "operations/3",
        OperationId(3),
        false,
    );
    recording.finished(&answer(3, 1, completed()), &ran_ok(), Some(action(9)), None);
    recording.submit(
        call(&recorder, "build-4", 4),
        "operations/4",
        OperationId(4),
        false,
    );
    recording.finished(
        &answer(4, 1, completed()),
        &Finished::TimedOut {
            status: status(Code::DeadlineExceeded),
            partial: Box::default(),
        },
        None,
        None,
    );

    let first = record(&recorder, "build-1").await;
    let row = first.row(&action(1)).expect("a row");
    assert_eq!(
        row.attempts[0].end,
        Some(AttemptEnd::Failed(Failure::Invalid, "bad image".to_owned()))
    );
    assert!(row.listing.is_none());
    let second = record(&recorder, "build-2").await;
    let row = second.row(&action(2)).expect("a row");
    assert_eq!(
        row.attempts[0].end,
        Some(AttemptEnd::Failed(Failure::Infra, String::new()))
    );
    let stored = record(&recorder, "build-3").await;
    assert!(stored.row(&action(3)).expect("a row").listing.is_some());
    let partial = record(&recorder, "build-4").await;
    assert!(partial.row(&action(4)).expect("a row").listing.is_some());
}

/// Asserts the two ends no lease of this scheduler made: an operation another frontend ran
/// (the end has no attempt and no listing, and answers as the callers were told) and one the
/// scheduler refused (answered with its reason). Catches: either end given a lease it did not
/// have, or a refusal that loses its reason.
#[tokio::test(start_paused = true)]
async fn an_end_without_a_lease_has_no_attempt_and_a_refusal_carries_its_reason() {
    let (mut recording, recorder) = rig();
    recording.submit(
        call(&recorder, "build-1", 1),
        "operations/1",
        OperationId(1),
        false,
    );
    recording.followed(OperationId(1), &ran_ok());
    recording.submit(
        call(&recorder, "build-2", 2),
        "operations/2",
        OperationId(2),
        false,
    );
    recording.refused(&Refusal {
        operation: OperationId(2),
        waiters: Vec::new(),
        reason: "no node can run it".to_owned(),
    });
    let followed = record(&recorder, "build-1").await;
    let row = followed.row(&action(1)).expect("a row");
    assert!(row.attempts.is_empty());
    assert!(row.listing.is_none());
    assert_eq!(row.outcome, Some(Answered::Ok { exit_code: 0 }));
    let refused = record(&recorder, "build-2").await;
    let row = refused.row(&action(2)).expect("a row");
    assert_eq!(
        row.outcome,
        Some(Answered::Refused("no node can run it".to_owned()))
    );
}
