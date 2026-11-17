//! What the executor tells the invocation recorder, read back from the record it builds.

use kbf_auth::AuthenticationMetadata;
use kbf_front::{CallMetadata, ExecuteCall, InvocationId as FrontId, Recorder};
use kbf_invocation::{AttemptEnd, How, InvocationId, Outcome, RecordKey};
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_types::{Digest, DigestFunction};

use super::*;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn call(recorder: &Recorder, id: &str) -> Option<ExecuteCall> {
    let metadata = CallMetadata {
        invocation_id: FrontId::parse(id),
        ..CallMetadata::default()
    };
    ExecuteCall::new(
        recorder,
        &metadata,
        &AuthenticationMetadata::default(),
        "main",
        digest(1),
        digest(2),
    )
}

fn recording() -> Recorder {
    let (recorder, recording) = Recorder::new(1 << 20, 256);
    tokio::spawn(recording.run());
    recorder
}

async fn row_of(recorder: &Recorder, id: &str) -> kbf_invocation::ActionRow {
    recorder.caught_up().await;
    let record = recorder
        .get(&RecordKey::owner(InvocationId::new(id)))
        .expect("a record");
    record.rows()[0].clone()
}

fn status(code: tonic::Code, message: &str) -> Finished {
    Finished::Failed(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// A call without an invocation id, or with the recorder off, is not recorded, and tells no
/// event; a recorded call is told as queued or joined.
#[tokio::test]
async fn only_a_call_with_an_invocation_id_is_recorded_as_queued_or_joined() {
    let off = Recorder::off();
    assert!(call(&off, "x").is_none());
    assert!(!answered(&off, None, "operations/1", OperationId(1), false));
    let recorder = recording();
    assert!(call(&recorder, "").is_none(), "no id, no call");
    assert!(!answered(
        &recorder,
        None,
        "operations/1",
        OperationId(1),
        false
    ));
    assert!(answered(
        &recorder,
        call(&recorder, "a"),
        "operations/1",
        OperationId(1),
        false
    ));
    assert!(answered(
        &recorder,
        call(&recorder, "b"),
        "operations/1",
        OperationId(1),
        true
    ));
    let (queued, joined) = (row_of(&recorder, "a").await, row_of(&recorder, "b").await);
    assert_eq!(queued.how, How::Ran);
    assert_eq!(joined.how, How::Joined);
    assert_eq!(queued.operation.as_deref(), Some("operations/1"));
    assert_eq!(queued.command, Some(digest(2)));
}

/// An attempt is a lease named by its operation and number, whose worker is its job: granted
/// when submitted, started when read running, requeued when the next is submitted, and ended
/// accepted when its result answers.
#[tokio::test]
async fn each_run_is_an_attempt_granted_started_requeued_and_accepted() {
    let recorder = recording();
    let op = OperationId(7);
    assert!(answered(
        &recorder,
        call(&recorder, "a"),
        "operations/7",
        op,
        false
    ));
    let (first, second) = (RunId::new(op, 0), RunId::new(op, 1));
    granted(&recorder, first, "k1-7-0");
    started(&recorder, first);
    requeued(&recorder, first);
    granted(&recorder, second, "k1-7-1");
    let ran = Finished::Ran(Box::new(ActionResult {
        exit_code: 3,
        ..ActionResult::default()
    }));
    finished(&recorder, op, Some(second), &ran);
    let row = row_of(&recorder, "a").await;
    let [one, two] = &row.attempts[..] else {
        panic!("two attempts: {:?}", row.attempts)
    };
    assert_eq!(one.lease, LeaseId::new(7, 0));
    assert_eq!(one.worker, WorkerId::new("k1-7-0"));
    assert!(one.started.is_some());
    assert_eq!(one.end, Some(AttemptEnd::Requeued(RequeueReason::Infra)));
    assert_eq!(two.lease, LeaseId::new(7, 1));
    assert_eq!(two.worker, WorkerId::new("k1-7-1"));
    assert!(two.started.is_none());
    assert_eq!(two.end, Some(AttemptEnd::Accepted));
    assert_eq!(row.outcome, Some(Outcome::Ok { exit_code: 3 }));
    assert!(row.listing.is_some(), "the result is listed");
}

/// The record's reason for a failed run is the failure its status code says, and a timeout is
/// a timeout whether the answer says so as a status or with its partial result.
#[tokio::test]
async fn a_failed_answer_is_recorded_with_the_failure_its_code_says() {
    let cases = [
        (
            status(tonic::Code::DeadlineExceeded, "slow"),
            Failure::Timeout,
            Outcome::TimedOut,
        ),
        (
            status(tonic::Code::InvalidArgument, "bad"),
            Failure::Invalid,
            Outcome::Failed {
                code: tonic::Code::InvalidArgument as i32,
                message: "bad".to_owned(),
            },
        ),
        (
            status(tonic::Code::FailedPrecondition, "oom"),
            Failure::OutOfMemory,
            Outcome::Failed {
                code: tonic::Code::FailedPrecondition as i32,
                message: "oom".to_owned(),
            },
        ),
        (
            status(tonic::Code::Unavailable, "gone"),
            Failure::Infra,
            Outcome::Failed {
                code: tonic::Code::Unavailable as i32,
                message: "gone".to_owned(),
            },
        ),
        (
            Finished::TimedOut {
                status: rpc::Status {
                    code: tonic::Code::DeadlineExceeded as i32,
                    message: "cut".to_owned(),
                    details: Vec::new(),
                },
                partial: Box::new(ActionResult::default()),
            },
            Failure::Timeout,
            Outcome::TimedOut,
        ),
    ];
    for (n, (answer, failure, outcome)) in cases.into_iter().enumerate() {
        let recorder = recording();
        let id = format!("c{n}");
        let op = OperationId(n as u64 + 1);
        answered(&recorder, call(&recorder, &id), "operations/x", op, false);
        let run = RunId::new(op, 0);
        granted(&recorder, run, "job");
        finished(&recorder, op, Some(run), &answer);
        let row = row_of(&recorder, &id).await;
        assert_eq!(row.outcome, Some(outcome), "case {n}");
        let end = row.attempts[0].end.clone();
        let Some(AttemptEnd::Failed(got, _)) = end else {
            panic!("case {n}: {end:?}")
        };
        assert_eq!(got, failure, "case {n}");
    }
}

/// An operation another frontend ended, or one that never submitted, ends with no lease.
#[tokio::test]
async fn an_operation_no_run_of_this_process_answered_ends_with_no_lease() {
    let recorder = recording();
    let op = OperationId(4);
    answered(&recorder, call(&recorder, "a"), "operations/4", op, false);
    finished(
        &recorder,
        op,
        None,
        &status(tonic::Code::Cancelled, "cancelled"),
    );
    let row = row_of(&recorder, "a").await;
    assert!(row.attempts.is_empty());
    assert_eq!(
        row.outcome,
        Some(Outcome::Failed {
            code: tonic::Code::Cancelled as i32,
            message: "cancelled".to_owned()
        })
    );
}
