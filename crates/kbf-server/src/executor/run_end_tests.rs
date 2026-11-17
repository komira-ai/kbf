//! How a run ends: the endings the job service reports, what the ingest says of the pack, and
//! what the callers are answered.

use std::time::Duration;

use js_proto::v1::{JobState, ResultReason};
use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use kbf_proto::google::rpc;
use tonic::Code;

use super::*;
use crate::executor::tests::{FakeIngest, Rig, error_info, job_ended, result_of};
use crate::executor::{OUT_OF_MEMORY_REASON, RejectReason};

fn status_of(finished: Finished) -> rpc::Status {
    match finished {
        Finished::Failed(status) => status,
        other => panic!("not a failure: {other:?}"),
    }
}

/// Catches an action's own timeout run again: it is answered DEADLINE_EXCEEDED at once, from
/// the one job.
#[tokio::test(start_paused = true)]
async fn a_timeout_is_answered_at_once_from_the_one_job() {
    let rig = Rig::start().await;
    rig.fake.inject_on("GetJob", job_ended(JobState::Failed));
    rig.fake.inject_on(
        "GetResult",
        result_of(
            JobState::Failed,
            ResultReason::WallLimit,
            b"the tool was killed",
        ),
    );
    let caller = rig.submit(1);
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::DeadlineExceeded as i32);
    assert_eq!(status.message, "the action ran out of time");
    let info = error_info(&status, "JOB_OUTPUT_TAIL").expect("its output is attached");
    assert_eq!(info.metadata["stderr"], "the tool was killed");
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"timeout\"} "),
        1
    );
}

/// Catches a job the service refuses as the caller's fault run again: a wall limit above the
/// service's ceiling is INVALID_ARGUMENT at once, with one submit and no read.
#[tokio::test(start_paused = true)]
async fn an_invalid_job_is_answered_at_once_from_its_refused_submit() {
    let rig = Rig::start().await;
    rig.fake.set_wall_ceiling(Duration::from_secs(10));
    let caller = rig.submit(1);
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::InvalidArgument as i32);
    assert!(
        status.message.contains("refused the job"),
        "{}",
        status.message
    );
    assert_eq!(rig.calls_of("SubmitJob"), 1);
    assert_eq!(rig.calls_of("GetJob"), 0);
    assert!(rig.fake.jobs().is_empty());
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"invalid\"} "),
        1
    );
}

/// Catches an out-of-memory kill answered as a plain failure: it is run again, and at the last
/// attempt answered FAILED_PRECONDITION with the reason ACTION_OUT_OF_MEMORY.
#[tokio::test(start_paused = true)]
async fn out_of_memory_is_run_again_then_answered_failed_precondition_with_its_reason() {
    let rig = Rig::start().await;
    for _ in 0..3 {
        rig.fake.inject_on("GetJob", job_ended(JobState::Failed));
        rig.fake.inject_on(
            "GetResult",
            result_of(JobState::Failed, ResultReason::Memory, b"killed"),
        );
    }
    let caller = rig.submit(1);
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    assert!(error_info(&status, OUT_OF_MEMORY_REASON).is_some());
    assert_eq!(rig.fake.jobs().len(), 3);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"out_of_memory\"} "),
        3
    );
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 2);
}

/// A job in a state this server does not know has its result read, and runs again.
#[tokio::test(start_paused = true)]
async fn a_job_in_a_state_this_server_does_not_know_is_judged_by_its_result() {
    let rig = Rig::start().await;
    rig.fake
        .inject_on("GetJob", job_ended(JobState::Unspecified));
    rig.fake.inject_on(
        "GetResult",
        result_of(JobState::Succeeded, ResultReason::Exit, b""),
    );
    let caller = rig.submit(1);
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}

/// The job's output could not be read: the answer says so, in its note, and keeps its code.
#[tokio::test(start_paused = true)]
async fn output_that_could_not_be_read_is_noted_in_the_answer() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    for attempt in 0..3 {
        rig.fake.inject_on(
            "GetLogs",
            Fault::Status {
                code: Code::Internal,
                message: "logs broke".to_owned(),
            },
        );
        let job = rig.submitted(&caller, attempt).await;
        rig.host_runs(&job);
        assert!(rig.fake.lose(&job));
    }
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    let info = error_info(&status, "JOB_OUTPUT_TAIL").expect("the note is attached");
    assert!(
        info.metadata["note"].contains("could not be read"),
        "{:?}",
        info.metadata
    );
}

// ---- the ingest

/// A pack the ingest refuses runs the operation again, and is counted by its reason; at the
/// last attempt the callers are answered UNAVAILABLE.
#[tokio::test(start_paused = true)]
async fn a_pack_the_ingest_refuses_is_run_again_and_counted_by_its_reason() {
    let rig = Rig::start().await;
    rig.ingest
        .script
        .lock()
        .unwrap()
        .push_back(Ingested::Reject(RejectReason::Malformed));
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    rig.host_finishes(&first, 0);
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(
        rig.counted("kbf_executor_ingests_total{result=\"rejected_malformed\"} "),
        1
    );
    assert_eq!(rig.counted("kbf_executor_ingests_total{result=\"ok\"} "), 1);
}

/// A store the ingest cannot read is run again too; three of them end in UNAVAILABLE.
#[tokio::test(start_paused = true)]
async fn a_pack_that_cannot_be_read_ends_in_unavailable_after_the_attempts() {
    let rig = Rig::start().await;
    for _ in 0..3 {
        rig.ingest
            .script
            .lock()
            .unwrap()
            .push_back(Ingested::Unavailable("store down".to_owned()));
    }
    let caller = rig.submit(1);
    for attempt in 0..3 {
        let job = rig.submitted(&caller, attempt).await;
        rig.host_finishes(&job, 0);
    }
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(status.message, "the pack could not be read: store down");
    assert_eq!(
        rig.counted("kbf_executor_ingests_total{result=\"unavailable\"} "),
        3
    );
}

/// A job that exited nonzero is not ingested: it is the task tool failing, and runs again.
#[tokio::test(start_paused = true)]
async fn a_job_that_exited_nonzero_is_run_again_and_its_pack_is_not_read() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    rig.host_finishes(&first, 3);
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.ingest.ingested.lock().unwrap().len(), 1);
    let _ = FakeIngest::ran(0);
}

// ---- the result

/// A result the service no longer has, for a job that ended, runs the operation again.
#[tokio::test(start_paused = true)]
async fn a_result_the_service_no_longer_has_runs_the_next_attempt() {
    let rig = Rig::start().await;
    rig.fake.inject_on(
        "GetResult",
        Fault::Status {
            code: Code::NotFound,
            message: "gone".to_owned(),
        },
    );
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    rig.host_finishes(&first, 0);
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 1);
}

/// A read of the result that fails is asked again at the next poll.
#[tokio::test(start_paused = true)]
async fn a_failed_read_of_the_result_is_asked_again_at_the_next_poll() {
    let rig = Rig::start().await;
    rig.fake.inject_on(
        "GetResult",
        Fault::Status {
            code: Code::Internal,
            message: "broke".to_owned(),
        },
    );
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.calls_of("GetResult"), 2);
    assert_eq!(rig.fake.jobs().len(), 1);
}
