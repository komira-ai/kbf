//! A run whose action tree is still being written: every attempt submits its job only once the
//! tree is stored, its row written meanwhile; a failed write runs the operation again with no
//! job submitted; a cancel while the tree is written ends the run with no job.

use std::sync::atomic::Ordering;

use kbf_front::{Dispatch, Finished};
use tonic::Code;

use super::*;
use crate::executor::MintError;
use crate::executor::OperationId;
use crate::executor::rows::OpRow;
use crate::executor::tests::{Rig, settle};

/// The runs whose tree writes were waited for, in order.
fn asked(rig: &Rig) -> Vec<RunId> {
    rig.ingest.stored_asked.lock().unwrap().clone()
}

/// The runs whose tree was answered stored, in order.
fn stored(rig: &Rig) -> Vec<RunId> {
    rig.ingest.stored_done.lock().unwrap().clone()
}

fn code_of(finished: &Finished) -> i32 {
    match finished {
        Finished::Failed(status) | Finished::TimedOut { status, .. } => status.code,
        Finished::Ran(_) => panic!("the action ran: {finished:?}"),
    }
}

/// Catches a job submitted before its tree is stored, on the first attempt or a later one (the
/// host would open a tree the store does not have and fail the start): while each attempt's
/// tree writes are held no `SubmitJob` reaches the service over four polls, and each attempt's
/// job is at the service only after its tree was answered stored.
#[tokio::test(start_paused = true)]
async fn every_attempt_submits_its_job_only_once_its_tree_is_stored() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    let (run0, run1) = (
        RunId::new(caller.operation(), 0),
        RunId::new(caller.operation(), 1),
    );
    rig.until("the first run waits for its tree", || asked(&rig) == [run0])
        .await;
    rig.advance(rig.cfg.poll * 4).await;
    assert_eq!(rig.calls_of("SubmitJob"), 0, "no submit before the tree");
    assert_eq!(stored(&rig), []);
    rig.ingest.release.notify_one();
    let first = rig.submitted(&caller, 0).await;
    assert_eq!(stored(&rig), [run0], "stored before its submit");
    assert_eq!(rig.calls_of("SubmitJob"), 1);
    rig.host_runs(&first);
    assert!(rig.fake.lose(&first));
    rig.until("the second run waits for its tree", || {
        asked(&rig) == [run0, run1]
    })
    .await;
    rig.advance(rig.cfg.poll * 4).await;
    assert_eq!(
        rig.calls_of("SubmitJob"),
        1,
        "no second submit before the tree"
    );
    assert_eq!(stored(&rig), [run0]);
    rig.ingest.release.notify_one();
    let second = rig.submitted(&caller, 1).await;
    assert_eq!(stored(&rig), [run0, run1], "stored before its submit");
    assert_eq!(rig.calls_of("SubmitJob"), 2);
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(asked(&rig), [run0, run1], "each waited for once");
    assert_eq!(
        rig.counted("kbf_executor_step_seconds_count{step=\"tree_wait\"} "),
        2
    );
}

/// Catches a job submitted for a tree whose write failed: the first attempt submits nothing
/// and cancels nothing, the operation runs again, and only the second attempt's job is made.
#[tokio::test(start_paused = true)]
async fn a_failed_tree_write_submits_no_job_and_the_operation_runs_again() {
    let rig = Rig::start().await;
    rig.ingest
        .stored_script
        .lock()
        .unwrap()
        .push_back(MintError::Unavailable("the store failed".to_owned()));
    let caller = rig.submit(1);
    let second = rig.submitted(&caller, 1).await;
    let first = rig.job_of(&caller, 0);
    assert!(rig.fake.submitted(&first).is_none(), "no job for {first}");
    assert_eq!(rig.calls_of("SubmitJob"), 1);
    assert_eq!(rig.calls_of("CancelJob"), 0, "nothing to withdraw");
    assert_eq!(stored(&rig), [RunId::new(caller.operation(), 1)]);
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 1);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}

/// Catches a tree the store refuses taken for one of the farm's failures, or a job made for
/// it: the operation is not run again, no job is submitted, and the callers are answered
/// `INVALID_ARGUMENT`, as a refused mint answers them.
#[tokio::test(start_paused = true)]
async fn a_refused_tree_write_answers_invalid_argument_with_no_job() {
    let rig = Rig::start().await;
    rig.ingest
        .stored_script
        .lock()
        .unwrap()
        .push_back(MintError::Refused("over a limit".to_owned()));
    let caller = rig.submit(1);
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::InvalidArgument as i32);
    assert!(rig.fake.jobs().is_empty());
    assert_eq!(rig.calls_of("SubmitJob"), 0);
}

/// Catches a cancel that is lost while the first attempt waits for its tree, or a job submitted
/// after it: the callers are answered `CANCELLED` and no job is made.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_the_tree_is_written_answers_cancelled_with_no_job() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    rig.until("the run waits for its tree", || asked(&rig).len() == 1)
        .await;
    assert!(rig.exec.cancel(&caller.name));
    let finished = rig.until_done(&caller).await;
    assert_eq!(code_of(&finished), Code::Cancelled as i32);
    rig.ingest.release.notify_one();
    settle().await;
    assert!(rig.fake.jobs().is_empty(), "no job");
    assert_eq!(rig.calls_of("SubmitJob"), 0);
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
/// preparation's limit, after which the operation runs again, with no job of the first attempt.
#[tokio::test(start_paused = true)]
async fn tree_writes_that_never_end_are_held_to_the_preparation_limit() {
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    rig.until("the run waits for its tree", || asked(&rig).len() == 1)
        .await;
    rig.advance(rig.cfg.prepare_limit).await;
    rig.until("the next attempt waits for its tree", || {
        asked(&rig).len() == 2
    })
    .await;
    let first = rig.job_of(&caller, 0);
    assert!(rig.fake.submitted(&first).is_none(), "no job for {first}");
    assert!(rig.fake.jobs().is_empty(), "no job of the first attempt");
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        1
    );
}

/// Catches the row write moved behind the tree's writes (the latency kept from overlapping
/// them lost): while the tree is held the operation's row is already in the state store and
/// timed, and no job is submitted yet.
#[tokio::test(start_paused = true)]
async fn the_row_is_written_while_the_tree_is_written() {
    let rig = Rig::stored().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = rig.submit(1);
    rig.until("the row is written while the tree is held", || {
        matches!(rig.rows("reader").op(caller.operation()), Ok(Some(_)))
    })
    .await;
    assert_eq!(asked(&rig), [RunId::new(caller.operation(), 0)]);
    assert_eq!(stored(&rig), []);
    assert_eq!(rig.calls_of("SubmitJob"), 0);
    assert_eq!(
        rig.counted("kbf_executor_step_seconds_count{step=\"row\"} "),
        1
    );
    rig.ingest.release.notify_one();
    let job = rig.submitted(&caller, 0).await;
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// Catches a run that goes on, or runs the operation again, where another process owns its row
/// and its tree's write failed too: the operation is handed over (the owner runs it), with no
/// job and no further attempt here.
#[tokio::test(start_paused = true)]
async fn a_row_another_process_owns_hands_over_even_when_the_tree_write_failed() {
    let rig = Rig::stored().await;
    let theirs = rig.rows("p-theirs");
    let digest = crate::executor::tests::digest_of(1);
    theirs
        .put_op(
            OperationId(1),
            &OpRow {
                owner: String::new(),
                instance: "main".to_owned(),
                hash: digest.hash_hex(),
                size: digest.size_bytes,
                attempt: 0,
                job: "their-job".to_owned(),
                key: None,
                whole_machine: false,
            },
        )
        .expect("their row");
    rig.ingest
        .stored_script
        .lock()
        .unwrap()
        .push_back(MintError::Unavailable("the store failed".to_owned()));
    let caller = rig.submit(1);
    assert_eq!(caller.operation(), OperationId(1));
    rig.until("handed over", || rig.exec.open() == 0).await;
    assert!(rig.fake.calls().is_empty(), "no job of this process");
    assert_eq!(asked(&rig).len(), 1, "no further attempt");
    assert!(caller.finished().is_none(), "the callers follow the owner");
}
