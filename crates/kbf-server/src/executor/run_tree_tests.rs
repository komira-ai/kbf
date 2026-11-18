//! A run whose action tree is still being written: a first attempt submits its job while the
//! tree's writes go on and waits for them after the submit; a failed write cancels the job and
//! runs the operation again; a later attempt waits for its tree before it submits.

use std::sync::atomic::Ordering;

use js_proto::v1::JobState;
use kbf_front::{Dispatch, Finished};
use tonic::Code;

use super::*;
use crate::executor::MintError;
use crate::executor::tests::{Rig, settle};

/// The runs whose tree writes were waited for, in order.
fn asked(rig: &Rig) -> Vec<RunId> {
    rig.ingest.stored_asked.lock().unwrap().clone()
}

fn code_of(finished: &Finished) -> i32 {
    match finished {
        Finished::Failed(status) | Finished::TimedOut { status, .. } => status.code,
        Finished::Ran(_) => panic!("the action ran: {finished:?}"),
    }
}

/// Catches a submit held back by the tree's writes, or a job read back before its tree is
/// stored: the job is at the service while the writes are held, the job is not read back until
/// they end, and the run then goes on to its answer.
#[tokio::test(start_paused = true)]
async fn a_first_attempt_submits_while_its_tree_is_written() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let run = RunId::new(caller.operation(), 0);
    rig.until("the run waits for its tree", || asked(&rig) == [run])
        .await;
    rig.advance(rig.cfg.poll * 4).await;
    assert_eq!(rig.calls_of("GetJob"), 0, "not read back before its tree");
    assert_eq!(rig.calls_of("SubmitJob"), 1);
    rig.ingest.release.notify_one();
    rig.host_finishes(&job, 0);
    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(ref result) if result.exit_code == 0));
    assert_eq!(asked(&rig), [run], "waited for once");
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(
        rig.counted("kbf_executor_step_seconds_count{step=\"tree_wait\"} "),
        1
    );
}

/// Catches a job left to fail on its host when its tree cannot be written, or a later attempt
/// that submits before its tree is stored: the first job is cancelled once its tree's write
/// fails, the operation runs again, and the second attempt's job is not submitted until its
/// own tree's writes end.
#[tokio::test(start_paused = true)]
async fn a_failed_tree_write_cancels_the_job_and_the_next_attempt_waits_for_its_tree() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    rig.ingest
        .stored_script
        .lock()
        .unwrap()
        .push_back(MintError::Unavailable("the store failed".to_owned()));
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    let (run0, run1) = (
        RunId::new(caller.operation(), 0),
        RunId::new(caller.operation(), 1),
    );
    rig.until("the first run waits for its tree", || asked(&rig) == [run0])
        .await;
    rig.ingest.release.notify_one();
    rig.until("the second run waits for its tree", || {
        asked(&rig) == [run0, run1]
    })
    .await;
    let second = rig.job_of(&caller, 1);
    assert_eq!(rig.fake.jobs()[0].state(), JobState::Cancelled, "{first}");
    assert_eq!(rig.calls_of("CancelJob"), 1);
    rig.advance(rig.cfg.poll * 4).await;
    assert!(
        rig.fake.submitted(&second).is_none(),
        "not submitted before its tree is stored"
    );
    rig.ingest.release.notify_one();
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 1);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}

/// Catches a tree the store refuses taken for one of the farm's failures: the job is
/// cancelled, the operation is not run again, and the callers are answered
/// `INVALID_ARGUMENT`, as a refused mint answers them.
#[tokio::test(start_paused = true)]
async fn a_refused_tree_write_answers_invalid_argument_with_one_job() {
    let rig = Rig::start().await;
    rig.ingest
        .stored_script
        .lock()
        .unwrap()
        .push_back(MintError::Refused("over a limit".to_owned()));
    let caller = rig.submit(1);
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::InvalidArgument as i32);
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(rig.fake.jobs()[0].state(), JobState::Cancelled);
}

/// Catches a cancel that is lost while the run waits for its tree: the submitted job is
/// cancelled and the callers are answered `CANCELLED`.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_the_tree_is_written_cancels_the_submitted_job() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.until("the run waits for its tree", || asked(&rig).len() == 1)
        .await;
    assert!(rig.exec.cancel(&caller.name));
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::Cancelled as i32);
    assert_eq!(rig.fake.jobs()[0].state(), JobState::Cancelled, "{job}");
}

/// Catches a later attempt that is not cancelled while it waits for its tree: a cancel then
/// ends the run with no second job.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_a_later_attempt_waits_for_its_tree_submits_nothing_more() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    rig.host_runs(&first);
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    assert!(rig.fake.lose(&first));
    rig.until("the second run waits for its tree", || {
        asked(&rig).len() == 2
    })
    .await;
    assert!(rig.exec.cancel(&caller.name));
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::Cancelled as i32);
    settle().await;
    assert_eq!(rig.fake.jobs().len(), 1, "no job of the second attempt");
}

/// Catches tree writes that never end holding a run for good: the wait is held to the
/// preparation's limit, after which the job is cancelled and the operation runs again.
#[tokio::test(start_paused = true)]
async fn tree_writes_that_never_end_are_held_to_the_preparation_limit() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    rig.submitted(&caller, 0).await;
    rig.until("the run waits for its tree", || asked(&rig).len() == 1)
        .await;
    rig.advance(rig.cfg.prepare_limit).await;
    rig.until("the next attempt waits for its tree", || {
        asked(&rig).len() == 2
    })
    .await;
    assert_eq!(rig.fake.jobs()[0].state(), JobState::Cancelled);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}
