//! How each ending of a job reaches the farm: which are the farm's fault and run again, which
//! are the caller's, and what the caller is answered.
//!
//! The fake job service ends a job by exit code only, so an ending for memory or for the wall
//! limit is scripted: the answers to the executor's reads of the job and of its result are
//! replaced by ones that say how it ended. The table of every ending is held to the code by
//! the unit tests of the outcome.

use bytes::Bytes;
use js_proto::v1::{
    GetJobResponse, GetResultResponse, Job, JobResult, JobState, ResultReason, StartFailureCode,
};
use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use kbf_server::executor::{Ingested, RejectReason};
use prost::Message as _;
use tonic::Code;

use crate::rig::{Caller, FakeIngest, Rig};

const HOST: &str = "host-1";

/// Submits the action of `tag` and returns its caller and the id of its job.
async fn submitted(rig: &mut Rig, tag: u8) -> (Caller, String) {
    rig.pump().await;
    let caller = rig.submit(tag);
    rig.pump().await;
    let jobs = rig.jobs_of(tag);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    (caller, jobs[0].id().to_owned())
}

fn exit_code(finished: &Finished) -> i32 {
    match finished {
        Finished::Ran(result) => result.exit_code,
        other => panic!("did not run: {other:?}"),
    }
}

/// The next read of the job `id` and of its result are answered as a job that failed for
/// `reason` with `exit_code`, whatever the job service holds.
fn script_failed(rig: &Rig, id: &str, reason: ResultReason, exit_code: i32) {
    let job = Job {
        id: id.to_owned(),
        state: JobState::Failed.into(),
        ..Job::default()
    };
    let result = JobResult {
        state: JobState::Failed.into(),
        exit_code: Some(exit_code),
        reason: reason.into(),
        ..JobResult::default()
    };
    let job = GetJobResponse { job: Some(job) };
    let result = GetResultResponse {
        result: Some(result),
    };
    rig.fake
        .inject_on("GetJob", Fault::Answer(Bytes::from(job.encode_to_vec())));
    rig.fake.inject_on(
        "GetResult",
        Fault::Answer(Bytes::from(result.encode_to_vec())),
    );
}

/// Catches a job killed at its memory limit being answered as the action's failure: it is the
/// farm's, so the action runs again with more memory (the farm records the kill), and the
/// caller sees only the second job's result.
#[tokio::test(start_paused = true)]
async fn an_out_of_memory_job_is_rerun_with_more_memory() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    let (caller, id) = submitted(&mut rig, 1).await;
    rig.host_runs(&id, HOST);
    script_failed(&rig, &id, ResultReason::Memory, 137);
    rig.pump().await;
    assert!(caller.finished().is_none(), "answered at the first kill");
    assert_eq!(
        rig.farm.memory_runs().len(),
        1,
        "the kill is on the farm's record"
    );
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "no rerun: {jobs:?}");
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
    let counted = rig.counted();
    for row in [
        "kbf_executor_runs_total{outcome=\"out_of_memory\"} 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }
}

/// Catches a job that passed its wall limit being rerun or called the farm's fault: the
/// action's timeout is the caller's, answered DEADLINE_EXCEEDED, and the action is not run
/// again.
#[tokio::test(start_paused = true)]
async fn a_timeout_is_answered_to_the_caller_and_not_rerun() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    let (caller, id) = submitted(&mut rig, 1).await;
    rig.host_runs(&id, HOST);
    script_failed(&rig, &id, ResultReason::WallLimit, 137);
    let finished = rig.until_done(&caller, 4).await;
    match finished {
        Finished::TimedOut { status, .. } | Finished::Failed(status) => {
            assert_eq!(Code::from_i32(status.code), Code::DeadlineExceeded);
        }
        other => panic!("not a timeout: {other:?}"),
    }
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
/// failure. Each is rerun as a new job, and the second job's OK is the caller's answer: the
/// failed attempts leave no result of the action behind, so one digest never shows a pass and
/// a failure (a flake) for an infrastructure fault.
#[tokio::test(start_paused = true)]
async fn an_infrastructure_ending_is_rerun_and_leaves_no_result_behind() {
    for ending in [
        Ending::Exits(12),
        Ending::NotStarted(StartFailureCode::NoCapacity),
        Ending::NotStarted(StartFailureCode::StartTimeout),
        Ending::NotStarted(StartFailureCode::ExclusiveNotReady),
        Ending::NotStarted(StartFailureCode::AgentRefused),
        Ending::NotStarted(StartFailureCode::Unspecified),
        Ending::Cancelled,
    ] {
        let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
        let (caller, id) = submitted(&mut rig, 1).await;
        match &ending {
            Ending::Exits(code) => rig.host_finishes(&id, HOST, *code),
            Ending::NotStarted(code) => assert!(rig.fake.fail_to_start(&id, *code), "{ending:?}"),
            Ending::Cancelled => {
                rig.api.cancel_job(&id).await.expect("cancelled");
            }
        }
        rig.pump().await;
        assert!(
            caller.finished().is_none(),
            "{ending:?} answered the caller"
        );
        let jobs = rig.jobs_of(1);
        assert_eq!(jobs.len(), 2, "{ending:?} was not rerun: {jobs:?}");
        rig.host_finishes(jobs[1].id(), HOST, 0);
        let finished = rig.until_done(&caller, 4).await;
        assert_eq!(exit_code(&finished), 0, "{ending:?}");
        assert!(rig.farm.memory_runs().is_empty(), "{ending:?}");
        assert_eq!(
            rig.ingest.calls.lock().expect("calls").len(),
            1,
            "{ending:?}"
        );
    }
}

/// Catches a job lost with its host being waited on, answered to the caller or charged to the
/// action as a kill: the host's fault is the farm's, so the action runs again as another job
/// of a new farm lease, and the caller is answered by that job alone.
#[tokio::test(start_paused = true)]
async fn a_lost_job_is_rerun_as_another_job() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    let (caller, id) = submitted(&mut rig, 1).await;
    rig.host_runs(&id, HOST);
    assert!(rig.fake.lose(&id), "{id} is lost");
    rig.pump().await;
    assert!(
        caller.finished().is_none(),
        "the lost job answered the caller"
    );
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "no rerun: {jobs:?}");
    assert_ne!(jobs[1].id(), id, "the lost job was taken for the new one");
    assert_ne!(jobs[1].arg("--lease"), jobs[0].arg("--lease"));
    assert!(rig.farm.memory_runs().is_empty(), "the loss was charged");
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
    let row = "kbf_executor_runs_total{outcome=\"lost\"} 1\n";
    let counted = rig.counted();
    assert!(counted.contains(row), "{row} in {counted}");
}

/// Catches an action that ran and failed (the task tool exited 0, the pack says the action
/// exited 3) being rerun or called an infrastructure failure: the farm answers the caller with
/// the action's own exit code, from the pack the frontend verified.
#[tokio::test(start_paused = true)]
async fn an_action_that_failed_is_answered_with_its_own_exit_code() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.ingest
        .script
        .lock()
        .expect("script")
        .push_back(FakeIngest::ran(3));
    let (caller, id) = submitted(&mut rig, 1).await;
    rig.host_finishes(&id, HOST, 0);
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 3);
    assert_eq!(rig.jobs_of(1).len(), 1);
}

/// Catches a pack the frontend refuses, or cannot read, being the action's failure or being
/// asked for again under the same key (the store is create-only, so a second write to it
/// would be refused): the action runs again as a new job with a key of its own.
#[tokio::test(start_paused = true)]
async fn a_refused_pack_is_rerun_under_a_new_key() {
    for refusal in [
        Ingested::Reject(RejectReason::HashMismatch),
        Ingested::Unavailable("the store is down".to_owned()),
    ] {
        let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
        rig.ingest.script.lock().expect("script").push_back(refusal);
        let (caller, id) = submitted(&mut rig, 1).await;
        rig.host_finishes(&id, HOST, 0);
        rig.pump().await;
        assert!(caller.finished().is_none());
        let jobs = rig.jobs_of(1);
        assert_eq!(jobs.len(), 2, "{jobs:?}");
        rig.host_finishes(jobs[1].id(), HOST, 0);
        assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
        let keys: Vec<String> = rig
            .ingest
            .calls
            .lock()
            .expect("calls")
            .iter()
            .map(|call| call.1.clone())
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1], "the second read was of the first key");
    }
}

/// Catches an ingest that panics taking the executor down, or leaving the action hanging
/// (its result never formed): the job is an infrastructure failure, and the action runs again.
#[tokio::test(start_paused = true)]
async fn an_ingest_that_stops_is_an_infrastructure_failure() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.ingest
        .panics
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (caller, id) = submitted(&mut rig, 1).await;
    rig.host_finishes(&id, HOST, 0);
    rig.pump().await;
    assert!(caller.finished().is_none());
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    rig.host_finishes(jobs[1].id(), HOST, 0);
    assert_eq!(exit_code(&rig.until_done(&caller, 4).await), 0);
}
