//! The task of an operation: its runs, their jobs and attempts, a lost answer, and cancel.

use std::time::Duration;

use js_proto::v1::JobState;
use kbf_front::{Dispatch, Finished, Stage};
use kbf_job_client::fake::{Call, Fault};
use kbf_proto::google::rpc;
use tonic::Code;

use super::*;
use crate::executor::InputsError;
use crate::executor::outcome::RESUBMITS;
use crate::executor::tests::{
    Caller, FakeIngest, Rig, Setup, digest_of, error_info, settle, submission_of,
};

fn code_of(finished: &Finished) -> i32 {
    match finished {
        Finished::Failed(status) | Finished::TimedOut { status, .. } => status.code,
        Finished::Ran(_) => panic!("the action ran: {finished:?}"),
    }
}

fn status_of(finished: Finished) -> rpc::Status {
    match finished {
        Finished::Failed(status) => status,
        other => panic!("not a failure: {other:?}"),
    }
}

// ---- attempts

/// Catches a lost job that is not run again, or run again more than once: the operation's next
/// attempt is a new job with the next attempt number, and its answer is the callers'.
#[tokio::test(start_paused = true)]
async fn a_lost_job_is_resubmitted_once_as_the_next_attempt() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    rig.host_runs(&first);
    assert!(rig.fake.lose(&first));
    let second = rig.submitted(&caller, 1).await;
    assert_ne!(first, second);
    assert!(second.ends_with("-1"), "attempt 1: {second}");
    assert_eq!(rig.fake.jobs().len(), 2);
    rig.host_finishes(&second, 0);
    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(ref result) if result.exit_code == 0));
    assert_eq!(rig.fake.jobs().len(), 2, "no third job");
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 1);
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 1);
    let ingested = rig.ingest.ingested.lock().unwrap().clone();
    assert_eq!(ingested.len(), 1, "only the second attempt's pack is read");
    assert_eq!(ingested[0].0, RunId::new(caller.operation(), 1));
    assert!(
        ingested[0]
            .1
            .contains(&format!("{}-1", caller.operation().0))
    );
}

/// Catches a bound that is not held: after the attempts it has, the callers are answered
/// UNAVAILABLE with the last job's output attached, and no further job is made.
#[tokio::test(start_paused = true)]
async fn the_attempts_bound_ends_in_unavailable_with_the_output_attached() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    for attempt in 0..=RESUBMITS {
        let job = rig.submitted(&caller, attempt).await;
        rig.host_runs(&job);
        assert!(
            rig.fake
                .write_logs(&job, b"out", format!("boom {attempt}").as_bytes())
        );
        assert!(rig.fake.lose(&job));
    }
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(status.message, "the job was lost with its host");
    let info = error_info(&status, "JOB_OUTPUT_TAIL").expect("the output is attached");
    assert_eq!(info.metadata["stderr"], format!("boom {RESUBMITS}"));
    assert_eq!(info.metadata["stdout"], "out");
    rig.advance(Duration::from_secs(5)).await;
    assert_eq!(rig.fake.jobs().len(), 3, "no job past the bound");
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 2);
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 3);
}

/// Catches a submit whose answer was lost being given up, or sent as another job: it is sent
/// again, the same spec under the same id, and the service makes one job.
#[tokio::test(start_paused = true)]
async fn a_submit_whose_answer_was_lost_is_resent_with_the_same_spec_and_counted() {
    let rig = Rig::start().await;
    rig.fake.lose_next_answer();
    let caller = rig.submit(1);
    rig.until("the submit is sent again", || {
        rig.calls_of("SubmitJob") >= 2
    })
    .await;
    let submits: Vec<_> = rig
        .fake
        .calls()
        .into_iter()
        .filter(|call| matches!(call, Call::SubmitJob(_)))
        .collect();
    assert_eq!(submits.len(), 2);
    assert_eq!(submits[0], submits[1], "the same spec under the same id");
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(
        rig.counted("kbf_executor_jobs_resubmitted_after_lost_answer_total "),
        1
    );
    assert_eq!(rig.counted("kbf_executor_jobs_submitted_total "), 1);
    let job = rig.job_of(&caller, 0);
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// A submit the job service refuses for a reason of its own (not the job's) runs the
/// operation's next attempt, under a new id.
#[tokio::test(start_paused = true)]
async fn a_submit_the_service_did_not_take_runs_the_next_attempt() {
    let rig = Rig::start().await;
    rig.fake.inject_on(
        "SubmitJob",
        Fault::Status {
            code: Code::ResourceExhausted,
            message: "full".to_owned(),
        },
    );
    let caller = rig.submit(1);
    let second = rig.submitted(&caller, 1).await;
    assert!(rig.fake.submitted(&rig.job_of(&caller, 0)).is_none());
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}

/// Catches a host's silence counted at every poll: it is counted once for a job.
#[tokio::test(start_paused = true)]
async fn a_host_that_missed_its_grace_is_counted_once_for_a_job() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    assert!(rig.fake.silence_host("h1"));
    for _ in 0..5 {
        rig.advance(rig.cfg.poll).await;
    }
    assert_eq!(rig.counted("kbf_executor_host_silence_seen_total "), 1);
    assert!(rig.fake.unsilence_host("h1"));
    rig.advance(rig.cfg.poll).await;
    assert!(rig.fake.silence_host("h1"));
    rig.advance(rig.cfg.poll).await;
    assert_eq!(rig.counted("kbf_executor_host_silence_seen_total "), 1);
    assert!(rig.fake.finish(&job, 0));
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// A read of the job that fails is tried again at the next poll, and does not run the
/// operation again.
#[tokio::test(start_paused = true)]
async fn a_failed_read_of_a_job_is_repeated_at_the_next_poll() {
    let rig = Rig::start().await;
    for _ in 0..2 {
        rig.fake.inject_on(
            "GetJob",
            Fault::Status {
                code: Code::Internal,
                message: "broke".to_owned(),
            },
        );
    }
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.until("the reads were tried", || rig.calls_of("GetJob") >= 3)
        .await;
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.fake.jobs().len(), 1);
}

/// A job the service no longer knows is lost to it: the operation runs again.
#[tokio::test(start_paused = true)]
async fn a_job_the_service_forgot_runs_again_as_the_next_attempt() {
    let rig = Rig::start().await;
    rig.fake.inject_on(
        "GetJob",
        Fault::Status {
            code: Code::NotFound,
            message: "no such job".to_owned(),
        },
    );
    let caller = rig.submit(1);
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 1);
}

// ---- preparing

/// Catches an input store that is down making a job: every attempt fails to prepare, no job is
/// submitted, and the callers are answered UNAVAILABLE after the attempts they have.
#[tokio::test(start_paused = true)]
async fn an_input_store_that_is_down_fails_every_attempt_and_submits_no_job() {
    let rig = Rig::start().await;
    *rig.inputs.facts.lock().unwrap() = Err(InputsError::Unavailable("cas down".to_owned()));
    let caller = rig.submit(1);
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(
        (status.code, status.message.as_str()),
        (Code::Unavailable as i32, "cas down")
    );
    assert!(status.details.is_empty(), "no job, no output");
    assert_eq!(
        rig.inputs
            .looked_up
            .load(std::sync::atomic::Ordering::SeqCst),
        3
    );
    assert!(
        rig.fake.calls().is_empty(),
        "the job service is never asked"
    );
}

/// An action that cannot run here is answered INVALID_ARGUMENT at once.
#[tokio::test(start_paused = true)]
async fn an_action_that_cannot_run_here_is_answered_invalid_at_once() {
    let rig = Rig::start().await;
    *rig.inputs.facts.lock().unwrap() = Err(InputsError::Invalid("not the base image".to_owned()));
    let caller = rig.submit(1);
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::InvalidArgument as i32);
    assert_eq!(
        rig.inputs
            .looked_up
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "not run again"
    );
}

/// Catches a preparation with no end (a hung CAS holding the task): past its limit it is a run
/// that ended for a reason of the farm's, again at each attempt.
#[tokio::test(start_paused = true)]
async fn a_preparation_past_its_limit_is_a_run_that_ended_for_the_farms_reason() {
    let rig = Rig::with(|cfg| cfg.prepare_limit = Duration::from_secs(7)).await;
    rig.inputs
        .hang
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let caller = rig.submit(1);
    settle().await;
    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert!(
        status.message.contains("longer than its limit"),
        "{}",
        status.message
    );
    assert_eq!(
        rig.inputs
            .looked_up
            .load(std::sync::atomic::Ordering::SeqCst),
        3
    );
    assert!(rig.fake.jobs().is_empty());
}

// ---- cancel

/// Catches a cancel that never reaches the job service: the job is cancelled there, and the
/// callers are answered CANCELLED.
#[tokio::test(start_paused = true)]
async fn cancel_reaches_the_job_service() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    assert!(rig.exec.cancel(&caller.name));
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::Cancelled as i32);
    let cancels: Vec<String> = rig
        .fake
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::CancelJob(request) => Some(request.job_id),
            _ => None,
        })
        .collect();
    assert_eq!(cancels, [job]);
    assert_eq!(rig.fake.jobs()[0].state(), JobState::Cancelled);
    assert_eq!(rig.counted("kbf_executor_jobs_cancelled_total "), 1);
    assert!(
        !rig.exec.cancel(&caller.name),
        "a finished operation is not open"
    );
    let kept = rig.exec.wait(&caller.name).expect("kept");
    assert_eq!(*kept.stage.borrow(), Stage::Done(finished));
}

/// A job that runs is cancelled on its host too, and a twin that joined is answered with it.
#[tokio::test(start_paused = true)]
async fn cancel_of_a_running_job_answers_every_caller_cancelled() {
    let rig = Rig::start().await;
    let (one, two) = (rig.submit(1), rig.submit(1));
    let job = rig.submitted(&one, 0).await;
    rig.host_runs(&job);
    rig.until("the callers see it executing", || {
        two.seen() == Stage::Executing
    })
    .await;
    assert!(rig.exec.cancel(&two.name));
    assert_eq!(code_of(&rig.until_done(&one).await), Code::Cancelled as i32);
    assert_eq!(code_of(&rig.until_done(&two).await), Code::Cancelled as i32);
    assert_eq!(rig.calls_of("CancelJob"), 1);
    assert!(rig.fake.host_stopped(&job), "the host was asked to stop");
}

/// A cancel before the submit stops the run, and asks nothing of the job service.
#[tokio::test(start_paused = true)]
async fn a_cancel_before_the_submit_stops_the_run_without_touching_the_job_service() {
    let rig = Rig::start().await;
    rig.inputs
        .hang
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let caller = rig.submit(1);
    settle().await;
    assert!(rig.exec.cancel(&caller.name));
    assert_eq!(
        code_of(&rig.until_done(&caller).await),
        Code::Cancelled as i32
    );
    assert!(rig.fake.calls().is_empty());
    assert_eq!(rig.exec.open(), 0);
}

/// A cancel while a submit is unanswered cancels the job anyway: the service may have taken it.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_a_submit_is_unanswered_cancels_the_job_it_may_have_taken() {
    let rig = Rig::start().await;
    rig.fake.inject_on("SubmitJob", Fault::Hang);
    let caller = rig.submit(1);
    rig.until("the submit is hung", || rig.calls_of("SubmitJob") == 1)
        .await;
    assert!(rig.exec.cancel(&caller.name));
    assert_eq!(
        code_of(&rig.until_done(&caller).await),
        Code::Cancelled as i32
    );
    let job = rig.job_of(&caller, 0);
    let cancelled: Vec<_> = rig
        .fake
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::CancelJob(request) => Some(request.job_id),
            _ => None,
        })
        .collect();
    assert_eq!(cancelled, [job]);
}

/// A cancel whose answer is lost is sent again, at most three times.
#[tokio::test(start_paused = true)]
async fn a_cancel_is_repeated_while_its_answer_is_lost_up_to_three_times() {
    let rig = Rig::start().await;
    let (one, two) = (rig.submit(1), rig.submit(2));
    rig.submitted(&one, 0).await;
    rig.submitted(&two, 0).await;
    rig.fake.inject_on("CancelJob", Fault::LoseAnswer);
    assert!(rig.exec.cancel(&one.name));
    rig.until_done(&one).await;
    assert_eq!(rig.calls_of("CancelJob"), 2, "once lost, once answered");
    for _ in 0..5 {
        rig.fake.inject_on("CancelJob", Fault::LoseAnswer);
    }
    assert!(rig.exec.cancel(&two.name));
    rig.until_done(&two).await;
    assert_eq!(
        rig.calls_of("CancelJob"),
        5,
        "three more, and then it gives up"
    );
    assert_eq!(rig.counted("kbf_executor_jobs_cancelled_total "), 1);
}

// ---- the record

/// Catches the events of a recorded call: it is told queued then joined, each run is an
/// attempt granted when submitted and started when seen running, an attempt that ends for the
/// farm's reason is requeued, and the operation ends with the result of the last.
#[tokio::test(start_paused = true)]
async fn the_record_events_of_a_recorded_call_follow_its_runs() {
    use kbf_auth::AuthenticationMetadata;
    use kbf_front::{CallMetadata, ExecuteCall, InvocationId as FrontId};
    use kbf_invocation::{AttemptEnd, How, InvocationId, Outcome, RecordKey, RequeueReason};
    let rig = Rig::build(
        Setup {
            store: false,
            record: true,
        },
        |_| {},
    )
    .await;
    let call = |id: &str| {
        let metadata = CallMetadata {
            invocation_id: FrontId::parse(id),
            ..CallMetadata::default()
        };
        ExecuteCall::new(
            &rig.recorder,
            &metadata,
            &AuthenticationMetadata::default(),
            "main",
            kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1),
            kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [2; 32], 2),
        )
    };
    let mut first = submission_of(digest_of(1));
    first.call = call("first");
    let mut twin = submission_of(digest_of(1));
    twin.call = call("twin");
    let caller = Caller::of(rig.exec.submit(first).expect("taken"));
    let joined = Caller::of(rig.exec.submit(twin).expect("taken"));
    let a = rig.submitted(&caller, 0).await;
    rig.host_runs(&a);
    rig.until("seen running", || caller.seen() == Stage::Executing)
        .await;
    assert!(rig.fake.lose(&a));
    let b = rig.submitted(&caller, 1).await;
    rig.host_finishes(&b, 0);
    rig.until_done(&caller).await;
    rig.until_done(&joined).await;
    rig.recorder.caught_up().await;
    let row = |id: &str| {
        let record = rig
            .recorder
            .get(&RecordKey::owner(InvocationId::new(id)))
            .expect("a record");
        record.rows()[0].clone()
    };
    let (ran, twin) = (row("first"), row("twin"));
    assert_eq!((ran.how, twin.how), (How::Ran, How::Joined));
    assert_eq!(ran.operation.as_deref(), Some(caller.name.as_str()));
    assert_eq!(twin.operation, ran.operation);
    let [one, two] = &ran.attempts[..] else {
        panic!("two attempts: {:?}", ran.attempts)
    };
    assert_eq!(one.worker.as_str(), a);
    assert!(one.started.is_some());
    assert_eq!(one.end, Some(AttemptEnd::Requeued(RequeueReason::Infra)));
    assert_eq!(two.worker.as_str(), b);
    assert_eq!(two.end, Some(AttemptEnd::Accepted));
    assert_eq!(ran.outcome, Some(Outcome::Ok { exit_code: 0 }));
    assert_eq!(
        twin.attempts.len(),
        2,
        "a joined call's attempts are its twin's"
    );
    let _ = FakeIngest::ran(0);
}

/// A cancel whose sender is gone never fires: the task waits for the job and not for a cancel.
#[tokio::test(start_paused = true)]
async fn a_cancel_that_can_never_come_is_not_a_cancel() {
    let (sender, mut cancel) = tokio::sync::watch::channel(false);
    drop(sender);
    let mut asked = std::pin::pin!(cancel_asked(&mut cancel));
    assert!(futures::poll!(&mut asked).is_pending());
    assert!(
        futures::poll!(&mut asked).is_pending(),
        "a dropped sender asked for a cancel"
    );
}

/// Catches an operation whose task broke left open for ever: its callers would wait with no
/// end, and the open operation would count against the memory guard. They are answered
/// INTERNAL and the operation is closed, so a new Execute of the action opens another.
#[tokio::test(start_paused = true)]
async fn an_operation_whose_task_breaks_is_answered_internal_and_closed() {
    let rig = Rig::start().await;
    rig.inputs
        .panics
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let caller = rig.submit(1);
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::Internal as i32);
    assert_eq!(rig.exec.open(), 0);
    rig.inputs
        .panics
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let again = rig.submit(1);
    assert_ne!(again.operation(), caller.operation());
}
