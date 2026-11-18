//! How each ending of a job reaches the caller: which are the farm's fault and run again, which
//! are the caller's, and what the caller is answered.
//!
//! The fake job service ends a job by exit code only, so an ending for memory or for the wall
//! limit is scripted: the answers to the executor's reads of the job and of its result are
//! replaced by ones that say how it ended. The table of every ending is held to the code by
//! the unit tests of the outcome.

use js_proto::v1::{ResultReason, StartFailureCode};
use kbf_front::Finished;
use kbf_server::executor::{Ingested, MintError, RejectReason};
use tonic::Code;

use crate::rig::{FakeIngest, HOST, Rig};

fn exit_code(finished: &Finished) -> i32 {
    match finished {
        Finished::Ran(result) => result.exit_code,
        other => panic!("did not run: {other:?}"),
    }
}

fn code(finished: &Finished) -> Code {
    match finished {
        Finished::Failed(status) | Finished::TimedOut { status, .. } => Code::from_i32(status.code),
        other => panic!("not a failure: {other:?}"),
    }
}

/// Catches a job killed at its memory limit being answered as the action's failure: it is the
/// farm's, so the action runs again as the operation's next attempt, and the caller sees only
/// the second job's result.
#[tokio::test]
async fn an_out_of_memory_job_is_run_again_and_the_caller_sees_the_second_result() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    rig.host_runs(first.id(), HOST);
    rig.script_failed(first.id(), ResultReason::Memory, 137);
    let second = rig.until_jobs(1, 2).await.remove(1);
    assert!(caller.finished().is_none(), "answered at the first kill");
    rig.host_finishes(second.id(), HOST, 0);
    assert_eq!(exit_code(&caller.done().await), 0);
    for row in [
        "kbf_executor_runs_total{outcome=\"out_of_memory\"} 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
    ] {
        let counted = rig.counted();
        assert!(counted.contains(row), "{row} in {counted}");
    }
}

/// Catches a job that passed its wall limit being run again or called the farm's fault: the
/// action's timeout is the caller's, answered DEADLINE_EXCEEDED, and the action is not run
/// again.
#[tokio::test]
async fn a_timeout_is_answered_to_the_caller_and_not_run_again() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    rig.host_runs(first.id(), HOST);
    rig.script_failed(first.id(), ResultReason::WallLimit, 137);
    let finished = caller.done().await;
    assert_eq!(code(&finished), Code::DeadlineExceeded, "{finished:?}");
    assert_eq!(rig.jobs_of(1).len(), 1);
}

/// How the fake ends a job that the infrastructure failed.
#[derive(Debug)]
enum Ending {
    /// The task tool exits with this nonzero code.
    Exits(i32),
    /// The host does not start the job, for this code.
    NotStarted(StartFailureCode),
    /// Someone else cancels the job.
    Cancelled,
}

/// Catches an ending that says the infrastructure failed (the task tool exited nonzero, the
/// job did not start, the job was cancelled by someone else) being answered as the action's
/// failure. Each is run again as a new job, and the second job's OK is the caller's answer: the
/// failed attempts leave no pack read behind, so one digest never shows a pass and a failure (a
/// flake) for an infrastructure fault.
#[tokio::test]
async fn an_infrastructure_ending_is_run_again_and_leaves_no_pack_read_behind() {
    for ending in [
        Ending::Exits(12),
        Ending::NotStarted(StartFailureCode::NoCapacity),
        Ending::NotStarted(StartFailureCode::StartTimeout),
        Ending::NotStarted(StartFailureCode::ExclusiveNotReady),
        Ending::NotStarted(StartFailureCode::AgentRefused),
        Ending::NotStarted(StartFailureCode::Unspecified),
        Ending::Cancelled,
    ] {
        let rig = Rig::start(&[HOST], |_| {}).await;
        let mut caller = rig.submit(1);
        let first = rig.first_job(1).await;
        match &ending {
            Ending::Exits(code) => rig.host_finishes(first.id(), HOST, *code),
            Ending::NotStarted(code) => {
                assert!(rig.fake.fail_to_start(first.id(), *code), "{ending:?}");
            }
            Ending::Cancelled => {
                rig.api.cancel_job(first.id()).await.expect("cancelled");
            }
        }
        let second = rig.until_jobs(1, 2).await.remove(1);
        assert!(
            caller.finished().is_none(),
            "{ending:?} answered the caller"
        );
        rig.host_finishes(second.id(), HOST, 0);
        assert_eq!(exit_code(&caller.done().await), 0, "{ending:?}");
        assert_eq!(rig.ingest.keys_read().len(), 1, "{ending:?}");
        assert_eq!(rig.jobs_of(1).len(), 2, "{ending:?}");
    }
}

/// Catches a job lost with its host being waited on, answered to the caller or counted as the
/// action's failure: the host's fault is the farm's, so the action runs again as the next
/// attempt, a job of a new id, and the caller is answered by that job alone.
#[tokio::test]
async fn a_lost_job_is_run_again_as_another_job() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    rig.host_runs(first.id(), HOST);
    assert!(rig.fake.lose(first.id()), "{} is lost", first.id());
    let second = rig.until_jobs(1, 2).await.remove(1);
    assert!(
        caller.finished().is_none(),
        "the lost job answered the caller"
    );
    assert_ne!(
        second.id(),
        first.id(),
        "the lost job was taken for the new one"
    );
    assert_ne!(second.arg("--lease"), first.arg("--lease"));
    rig.host_finishes(second.id(), HOST, 0);
    assert_eq!(exit_code(&caller.done().await), 0);
    let row = "kbf_executor_runs_total{outcome=\"lost\"} 1\n";
    let counted = rig.counted();
    assert!(counted.contains(row), "{row} in {counted}");
}

/// Catches an operation that is run again for ever: three jobs ending for the farm's reason
/// (the first and the two runs again) answer the caller UNAVAILABLE, and no fourth job is made.
#[tokio::test]
async fn an_operation_whose_every_attempt_ends_for_the_farms_reason_is_answered_after_three_jobs() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    for attempt in 0..3 {
        let job = rig.until_jobs(1, attempt + 1).await.remove(attempt);
        rig.host_finishes(job.id(), HOST, 12);
    }
    let finished = caller.done().await;
    assert_eq!(code(&finished), Code::Unavailable, "{finished:?}");
    assert_eq!(rig.jobs_of(1).len(), 3, "a fourth job was made");
    assert!(rig.ingest.keys_read().is_empty());
}

/// Catches an action that ran and failed (the task tool exited 0, the pack says the action
/// exited 3) being run again or called an infrastructure failure: the caller is answered with
/// the action's own exit code, from the pack the frontend verified.
#[tokio::test]
async fn an_action_that_failed_is_answered_with_its_own_exit_code() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    rig.ingest
        .script
        .lock()
        .expect("script")
        .push_back(FakeIngest::ran(3));
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_finishes(job.id(), HOST, 0);
    assert_eq!(exit_code(&caller.done().await), 3);
    assert_eq!(rig.jobs_of(1).len(), 1);
}

/// Catches a pack the frontend refuses, or cannot read, being the action's failure or being
/// asked for again under the same key (the store is create-only, so a second write to it would
/// be refused): the action runs again as a new job with a key of its own.
#[tokio::test]
async fn a_refused_pack_is_run_again_under_a_new_key() {
    for refusal in [
        Ingested::Reject(RejectReason::HashMismatch),
        Ingested::Unavailable("the store is down".to_owned()),
    ] {
        let rig = Rig::start(&[HOST], |_| {}).await;
        rig.ingest.script.lock().expect("script").push_back(refusal);
        let mut caller = rig.submit(1);
        let first = rig.first_job(1).await;
        rig.host_finishes(first.id(), HOST, 0);
        let second = rig.until_jobs(1, 2).await.remove(1);
        rig.host_finishes(second.id(), HOST, 0);
        assert_eq!(exit_code(&caller.done().await), 0);
        let keys = rig.ingest.keys_read();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1], "the second read was of the first key");
    }
}

/// Catches a job that is killed at its memory limit at every attempt being answered as an
/// infrastructure failure: after the third kill the caller is answered FAILED_PRECONDITION
/// naming the out-of-memory reason, which a client tells from a flake, and no fourth job is made.
#[tokio::test]
async fn a_job_killed_for_memory_at_every_attempt_is_answered_out_of_memory() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    for attempt in 0..3 {
        let job = rig.until_jobs(1, attempt + 1).await.remove(attempt);
        rig.host_runs(job.id(), HOST);
        rig.script_failed(job.id(), ResultReason::Memory, 137);
    }
    let Finished::Failed(status) = caller.done().await else {
        panic!("not a failure");
    };
    assert_eq!(Code::from_i32(status.code), Code::FailedPrecondition);
    let detail = String::from_utf8_lossy(&status.details[0].value).into_owned();
    assert!(
        detail.contains(kbf_server::executor::OUT_OF_MEMORY_REASON),
        "{detail}"
    );
    assert_eq!(rig.jobs_of(1).len(), 3);
}

/// The SHA-256 of a manifest the store lost, as tierstore names it.
const LOST: &str = "9e1bf90c91fe575d7dacd35d1b21d058c2f1fd7abd0d23ffa41c4a0da2facb7f";

/// Catches a job that did not start because the store lost an object of its tree being run again
/// over the same stale state, or answered as the action's failure: the frontend is told the
/// object and the action once, before the next attempt is prepared, and the next job's OK is the
/// caller's answer; the run is counted `infra`.
#[tokio::test]
async fn a_job_that_did_not_start_for_a_lost_object_tells_the_frontend_and_runs_again() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    let detail = format!("cannot fetch manifest {LOST}: upstream 404");

    assert!(rig.fake.refuse_start(first.id(), &detail));
    let second = rig.until_jobs(1, 2).await.remove(1);
    let told = rig.ingest.lost.lock().expect("lock").clone();
    rig.host_finishes(second.id(), HOST, 0);

    assert_eq!(exit_code(&caller.done().await), 0);
    assert_eq!(told.len(), 1, "{told:?}");
    let hex: String = told[0].1.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, LOST);
    assert_eq!(
        rig.ingest.forms.lock().expect("lock").len(),
        2,
        "minted again"
    );
    let row = "kbf_executor_runs_total{outcome=\"infra\"} 1\n";
    let counted = rig.counted();
    assert!(counted.contains(row), "{row} in {counted}");
}

/// Catches inputs the store lost (found when the next attempt's tree is written) answered as
/// anything but REAPI's missing inputs, or run again before the client uploads them: the caller
/// is answered FAILED_PRECONDITION naming the blob, no job is made, and the run is counted
/// `missing`.
#[tokio::test]
async fn inputs_the_store_lost_are_answered_missing_without_a_job() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let blob = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [4; 32], 9);
    rig.ingest
        .mint_fails
        .lock()
        .expect("lock")
        .push_back(MintError::Missing(vec![blob]));
    let mut caller = rig.submit(1);

    let finished = caller.done().await;

    assert_eq!(code(&finished), Code::FailedPrecondition, "{finished:?}");
    let Finished::Failed(status) = &finished else {
        panic!("not failed: {finished:?}");
    };
    assert_eq!(status.message, "1 input blob(s) are not in the CAS");
    assert!(rig.jobs_of(1).is_empty(), "a job was made");
    let row = "kbf_executor_runs_total{outcome=\"missing\"} 1\n";
    let counted = rig.counted();
    assert!(counted.contains(row), "{row} in {counted}");
}
