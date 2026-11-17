//! The invocation record of an operation the executor ran, read back from the record the
//! executor's events build: the Execute call, its Command, one attempt for each job (its lease
//! is the operation and the attempt, its worker the job's id), the job read back running, the
//! attempt given up for the farm's reason, and the end.

use kbf_front::Finished;
use kbf_invocation::{AttemptEnd, How, InvocationId, Outcome, RecordKey, RequeueReason};
use kbf_types::{LeaseId, WorkerId};

use crate::rig::{HOST, Rig, digest_of};

/// The first row of the record of invocation `id`, once the recorder has applied every event.
async fn row_of(rig: &Rig, id: &str) -> kbf_invocation::ActionRow {
    rig.recorder.caught_up().await;
    let record = rig
        .recorder
        .get(&RecordKey::owner(InvocationId::new(id)))
        .expect("a record");
    record.rows()[0].clone()
}

/// The operation `name` (`operations/<id>`) is.
fn operation_of(name: &str) -> u64 {
    kbf_server::executor::parse_stored_name(name)
        .expect("an operation name")
        .0
}

/// Catches a direct operation whose record lacks what the build page shows: the call named by
/// its operation, the Command digest, one attempt whose lease is (operation, 0) and whose worker
/// is the job's id, a start stamp once the job was read running, an accepted end, and the
/// outcome and listing of the answer.
#[tokio::test]
async fn a_run_is_recorded_as_one_attempt_granted_to_its_job_started_and_accepted() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit_called(1, "inv-a");
    let job = rig.first_job(1).await;
    rig.host_runs(job.id(), HOST);
    caller.executing().await;
    assert!(rig.fake.finish(job.id(), 0));
    assert!(matches!(caller.done().await, Finished::Ran(_)));

    let row = row_of(&rig, "inv-a").await;
    let op = operation_of(&caller.name);
    assert_eq!(row.how, How::Ran);
    assert_eq!(row.operation.as_deref(), Some(caller.name.as_str()));
    assert_eq!(row.command, Some(digest_of(101)));
    let [attempt] = &row.attempts[..] else {
        panic!("one attempt: {:?}", row.attempts)
    };
    assert_eq!(attempt.lease, LeaseId::new(op, 0));
    assert_eq!(attempt.worker, WorkerId::new(job.id()));
    assert!(attempt.started.is_some(), "the job was read running");
    assert_eq!(attempt.end, Some(AttemptEnd::Accepted));
    assert_eq!(row.outcome, Some(Outcome::Ok { exit_code: 0 }));
    assert!(row.listing.is_some(), "the result is listed");
}

/// Catches an attempt given up for the farm's reason missing from the record, or its next
/// attempt sharing its lease: a job lost with its host is recorded as the first attempt,
/// requeued for an infrastructure reason, and the second job as the second attempt
/// (operation, 1), whose worker is the second job's id, accepted.
#[tokio::test]
async fn a_lost_job_is_recorded_as_a_requeued_attempt_beside_the_one_that_answered() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit_called(1, "inv-b");
    let first = rig.first_job(1).await;
    rig.host_runs(first.id(), HOST);
    assert!(rig.fake.lose(first.id()));
    let second = rig.until_jobs(1, 2).await.remove(1);
    rig.host_finishes(second.id(), HOST, 0);
    assert!(matches!(caller.done().await, Finished::Ran(_)));

    let row = row_of(&rig, "inv-b").await;
    let op = operation_of(&caller.name);
    let [one, two] = &row.attempts[..] else {
        panic!("two attempts: {:?}", row.attempts)
    };
    assert_eq!(one.lease, LeaseId::new(op, 0));
    assert_eq!(one.worker, WorkerId::new(first.id()));
    assert_eq!(one.end, Some(AttemptEnd::Requeued(RequeueReason::Infra)));
    assert_eq!(two.lease, LeaseId::new(op, 1));
    assert_eq!(two.worker, WorkerId::new(second.id()));
    assert_eq!(two.end, Some(AttemptEnd::Accepted));
    assert_eq!(row.outcome, Some(Outcome::Ok { exit_code: 0 }));
}

/// Catches a call that joined a running operation being recorded as a second run: two calls of
/// the same action by two invocations make one job; the first's row is a run with one attempt,
/// the second's is a join of the same operation with none of its own, and both end with the
/// operation's outcome.
#[tokio::test]
async fn a_call_that_joins_a_running_operation_is_recorded_as_joined_with_no_attempt_of_its_own() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut first = rig.submit_called(1, "inv-c");
    let mut second = rig.submit_called(1, "inv-d");
    assert_eq!(first.name, second.name, "one operation");
    let job = rig.first_job(1).await;
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(first.done().await, Finished::Ran(_)));
    assert!(matches!(second.done().await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(1).len(), 1);

    let (ran, joined) = (row_of(&rig, "inv-c").await, row_of(&rig, "inv-d").await);
    assert_eq!(ran.how, How::Ran);
    assert_eq!(joined.how, How::Joined);
    assert_eq!(joined.operation, ran.operation);
    assert_eq!(ran.attempts.len(), 1);
    assert_eq!(joined.attempts.len(), 1, "the operation's attempt is shown");
    assert_eq!(joined.outcome, Some(Outcome::Ok { exit_code: 0 }));
}

/// Catches the executor recording a call that carries no invocation id (the id-less
/// pseudo-invocation is not built: such a run is as unrecorded as before): an operation
/// submitted with no call runs to its end, and the recorder is told nothing of it.
#[tokio::test]
async fn an_operation_with_no_invocation_id_tells_the_recorder_nothing() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_runs(job.id(), HOST);
    caller.executing().await;
    rig.host_finishes_running(job.id());
    assert!(matches!(caller.done().await, Finished::Ran(_)));
    rig.recorder.caught_up().await;
    assert_eq!(rig.recorder.sent(), 0, "the recorder was told something");
}
