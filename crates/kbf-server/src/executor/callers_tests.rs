//! An operation no client waits on is cancelled once its grace runs out, and one a client comes
//! back to, here or on another frontend, is not. On the rig of the executor's tests: the job
//! service's fake, and the paused clock, moved by hand to either side of the grace.

use std::sync::atomic::Ordering;
use std::time::Duration;

use js_proto::v1::{JobSpec, JobState, NetworkLevel};
use kbf_front::{Dispatch, Finished};
use kbf_types::OperationId;
use tonic::Code;

use crate::executor::rows::OpRow;
use crate::executor::task::RunId;
use crate::executor::tests::{Caller, IMAGE, Rig, Setup, digest_of, locked, settle, submission_of};
use crate::executor::{Running, job_id, stored_name};

/// The grace of these tests: shorter than a job's wall limit at the fake (six minutes), so the
/// job is still running when it runs out.
const GRACE: Duration = Duration::from_secs(30);

/// Just under a whole grace.
const SHORT: Duration = GRACE.saturating_sub(Duration::from_millis(1));

const CANCELLED: &str = "kbf_executor_operations_cancelled_no_caller_total ";
const ALONE: &str = "kbf_executor_operations_no_caller ";

async fn rig() -> Rig {
    Rig::with(|cfg| cfg.no_caller_grace = GRACE).await
}

async fn stored() -> Rig {
    let setup = Setup {
        store: true,
        record: false,
    };
    Rig::build(setup, |cfg| cfg.no_caller_grace = GRACE).await
}

/// A caller submits the action of `tag`, whose job the host runs.
async fn running(rig: &Rig, tag: u8) -> (Caller, String) {
    let caller = rig.submit(tag);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    (caller, job)
}

/// `caller` leaves: its stream is dropped, and the watch of its operation sees it.
async fn leaves(caller: Caller) -> String {
    let name = caller.name.clone();
    drop(caller);
    settle().await;
    name
}

/// Moves the clock by `total` in steps of a second, as the tasks that read and write the state
/// store every poll see it go.
async fn pass(rig: &Rig, total: Duration) {
    let step = Duration::from_secs(1);
    let mut gone = Duration::ZERO;
    while gone < total {
        rig.advance(step).await;
        gone += step;
    }
}

fn code_of(finished: &Finished) -> i32 {
    match finished {
        Finished::Failed(status) => status.code,
        other => panic!("not a failure: {other:?}"),
    }
}

fn job_state(rig: &Rig, job: &str) -> JobState {
    let jobs = rig.fake.jobs();
    jobs.iter().find(|j| j.id == job).expect("the job").state()
}

/// The answer of the operation `name` asked for by name, once its task has given it.
async fn answer_of(rig: &Rig, name: &str) -> Finished {
    let kept = Caller::of(rig.exec.wait(name).expect("kept by name"));
    rig.until_done(&kept).await
}

// ---- one frontend

/// Catches a job left running with nobody to take its answer, or one cancelled before the
/// grace: the only caller leaves, and a millisecond before the grace has passed nothing is
/// cancelled and the operation counts as one with no caller; at the grace its job is cancelled
/// at the job service, the cancel counted, the operation answered CANCELLED by name, and no
/// longer counted as one with no caller.
#[tokio::test(start_paused = true)]
async fn the_only_callers_leaving_cancels_the_job_once_the_grace_runs_out() {
    let rig = rig().await;
    let (caller, job) = running(&rig, 1).await;
    assert_eq!(rig.counted(ALONE), 0, "a caller is on it");
    let name = leaves(caller).await;
    assert_eq!(rig.counted(ALONE), 1);

    rig.advance(SHORT).await;
    assert_eq!(
        rig.calls_of("CancelJob"),
        0,
        "cancelled before the grace ran out"
    );
    assert_eq!(rig.counted(CANCELLED), 0);

    rig.advance(Duration::from_millis(1)).await;
    assert_eq!(rig.calls_of("CancelJob"), 1);
    assert_eq!(job_state(&rig, &job), JobState::Cancelled);
    assert_eq!(rig.counted(CANCELLED), 1);
    assert_eq!(rig.counted("kbf_executor_jobs_cancelled_total "), 1);
    assert_eq!(rig.counted(ALONE), 0);
    assert_eq!(
        code_of(&answer_of(&rig, &name).await),
        Code::Cancelled as i32
    );
    assert_eq!(rig.exec.open(), 0);
}

/// Catches an Execute that comes back inside the grace and is not joined to the operation, or
/// whose operation is cancelled anyway, and a grace that is not started again at the next
/// leaving: the action's Execute a second before the grace joins the running operation, which
/// is left running past the grace; once that caller leaves too, a whole grace passes before the
/// job is cancelled.
#[tokio::test(start_paused = true)]
async fn an_execute_inside_the_grace_joins_the_operation_and_keeps_its_job() {
    let rig = rig().await;
    let (caller, job) = running(&rig, 1).await;
    let name = leaves(caller).await;
    rig.advance(GRACE - Duration::from_secs(1)).await;

    let again = rig.submit(1);
    assert_eq!(again.name, name, "the Execute joined the running operation");
    settle().await;
    assert_eq!(rig.counted(ALONE), 0);
    rig.advance(GRACE * 2).await;
    assert_eq!(
        rig.calls_of("CancelJob"),
        0,
        "a joined operation was cancelled"
    );
    assert_eq!(
        rig.calls_of("SubmitJob"),
        1,
        "the join submitted a second job"
    );

    leaves(again).await;
    rig.advance(SHORT).await;
    assert_eq!(
        rig.calls_of("CancelJob"),
        0,
        "the grace did not start again"
    );
    rig.advance(Duration::from_millis(1)).await;
    assert_eq!(rig.calls_of("CancelJob"), 1);
    assert_eq!(job_state(&rig, &job), JobState::Cancelled);
}

/// Catches a WaitExecution inside the grace that does not count as a caller: it keeps the job
/// running for as long as it is open.
#[tokio::test(start_paused = true)]
async fn a_wait_execution_inside_the_grace_keeps_the_job() {
    let rig = rig().await;
    let (caller, job) = running(&rig, 1).await;
    let name = leaves(caller).await;
    rig.advance(SHORT).await;

    let waiting = Caller::of(rig.exec.wait(&name).expect("open"));
    rig.advance(GRACE * 3).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert!(rig.fake.finish(&job, 0), "the job ran on");
    assert!(matches!(rig.until_done(&waiting).await, Finished::Ran(_)));
    assert_eq!(rig.counted(CANCELLED), 0);
}

/// Catches a grace that runs from the first leaving and not from the last: a GetOperation (a
/// caller for the length of its call) a second before the grace starts it again, so a whole
/// grace after that call the job is cancelled, and not before.
#[tokio::test(start_paused = true)]
async fn a_get_operation_inside_the_grace_starts_it_again() {
    let rig = rig().await;
    let (caller, _job) = running(&rig, 1).await;
    let name = leaves(caller).await;
    rig.advance(GRACE - Duration::from_secs(1)).await;

    drop(rig.exec.wait(&name).expect("open"));
    settle().await;
    rig.advance(SHORT).await;
    assert_eq!(
        rig.calls_of("CancelJob"),
        0,
        "the grace ran from the first leaving"
    );
    rig.advance(Duration::from_millis(1)).await;
    assert_eq!(rig.calls_of("CancelJob"), 1);
}

/// Catches a cancel when one of two callers leaves: the other keeps the operation, which is
/// not counted as one with no caller, and is answered when its job ends.
#[tokio::test(start_paused = true)]
async fn of_two_callers_one_leaving_keeps_the_job() {
    let rig = rig().await;
    let (one, job) = running(&rig, 1).await;
    let two = rig.submit(1);
    assert_eq!(one.name, two.name);
    leaves(one).await;
    rig.advance(GRACE * 2).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert_eq!(rig.counted(ALONE), 0);
    assert!(rig.fake.finish(&job, 0));
    assert!(matches!(rig.until_done(&two).await, Finished::Ran(_)));
}

/// Catches a result lost because its callers left: a job that ends inside the grace with no
/// caller has its pack read (where the frontend writes the action cache, when the action may be
/// cached) and its answer kept by name, and nothing is cancelled.
#[tokio::test(start_paused = true)]
async fn a_job_that_ends_with_no_caller_is_still_read_and_answered() {
    let rig = rig().await;
    let (caller, job) = running(&rig, 1).await;
    let operation = caller.operation();
    let name = leaves(caller).await;
    assert!(rig.fake.finish(&job, 0));
    rig.until("the pack is read", || {
        !locked(&rig.ingest.ingested).is_empty()
    })
    .await;
    rig.advance(GRACE * 2).await;

    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert_eq!(rig.counted(CANCELLED), 0);
    assert_eq!(
        rig.counted(ALONE),
        0,
        "a finished operation is no longer counted"
    );
    let ingested = locked(&rig.ingest.ingested).clone();
    assert_eq!(ingested.len(), 1);
    assert_eq!(ingested[0].0, RunId::new(operation, 0));
    assert!(matches!(answer_of(&rig, &name).await, Finished::Ran(_)));
}

/// Catches a grace of zero taken as "cancel at once": zero turns the cancel off, and an
/// operation with no caller is not counted either.
#[tokio::test(start_paused = true)]
async fn with_no_grace_an_operation_with_no_caller_runs_on() {
    let rig = Rig::with(|cfg| cfg.no_caller_grace = Duration::ZERO).await;
    let (caller, job) = running(&rig, 1).await;
    leaves(caller).await;
    assert_eq!(rig.counted(ALONE), 0);
    rig.advance(GRACE * 4).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert_eq!(job_state(&rig, &job), JobState::Running);
}

/// Catches an operation the watch cancelled that a new Execute still joins: the next Execute of
/// the action opens a new operation, as after a CancelOperation.
#[tokio::test(start_paused = true)]
async fn an_execute_after_the_cancel_opens_a_new_operation() {
    let rig = rig().await;
    let (caller, _job) = running(&rig, 1).await;
    let name = leaves(caller).await;
    rig.advance(GRACE).await;
    assert_eq!(rig.counted(CANCELLED), 1);

    let next = rig.submit(1);
    assert_ne!(next.name, name);
    rig.submitted(&next, 0).await;
    assert_eq!(rig.calls_of("SubmitJob"), 2);
}

// ---- over a state store

/// Catches a caller on another frontend that is not counted: a WaitExecution there keeps the
/// operation running here while it is open, the other frontend having written its row; once it
/// has gone, the operation is cancelled.
#[tokio::test(start_paused = true)]
async fn a_wait_execution_on_another_frontend_keeps_the_job() {
    let rig = stored().await;
    let (caller, job) = running(&rig, 1).await;
    let other = rig.executor("p-other");
    let remote = Caller::of(other.wait(&caller.name).expect("followed from the store"));
    rig.advance(rig.cfg.poll).await;
    let watched = rig.rows("reader").watched(caller.operation());
    assert_eq!(watched, Ok(true), "the other frontend wrote its row");
    leaves(caller).await;

    pass(&rig, GRACE * 3).await;
    assert_eq!(
        rig.calls_of("CancelJob"),
        0,
        "a caller on another frontend was not seen"
    );

    drop(remote);
    rig.until("the job is cancelled", || rig.calls_of("CancelJob") == 1)
        .await;
    assert_eq!(job_state(&rig, &job), JobState::Cancelled);
    assert_eq!(rig.counted(CANCELLED), 1);
}

/// Catches an Execute on another frontend that joined the operation through the store and is
/// not counted: it keeps the job running past the grace, and is answered when the job ends.
#[tokio::test(start_paused = true)]
async fn an_execute_on_another_frontend_that_joined_keeps_the_job() {
    let rig = stored().await;
    let (caller, job) = running(&rig, 1).await;
    let other = rig.executor("p-other");
    let joined = Caller::of(other.submit(submission_of(digest_of(1))).expect("taken"));
    rig.until("the other frontend joined", || {
        rig.counted("kbf_executor_operations_joined_total ") == 1
    })
    .await;
    leaves(caller).await;

    pass(&rig, GRACE * 3).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert!(rig.fake.finish(&job, 0));
    assert!(matches!(rig.until_done(&joined).await, Finished::Ran(_)));
    assert_eq!(rig.calls_of("SubmitJob"), 1, "one job for the action");
}

/// Catches a cancel made on what the store did not say: while the rows of other frontends'
/// callers cannot be read the operation is not cancelled; once they can, it is.
#[tokio::test(start_paused = true)]
async fn an_unreadable_store_cancels_nothing_until_it_is_read() {
    let rig = stored().await;
    let (caller, job) = running(&rig, 1).await;
    leaves(caller).await;
    let store = rig.store.as_ref().expect("a store");
    store.reads_fail.store(true, Ordering::SeqCst);
    pass(&rig, GRACE * 3).await;
    assert_eq!(rig.calls_of("CancelJob"), 0, "cancelled on a failed read");

    store.reads_fail.store(false, Ordering::SeqCst);
    rig.until("the job is cancelled", || rig.calls_of("CancelJob") == 1)
        .await;
    assert_eq!(job_state(&rig, &job), JobState::Cancelled);
}

/// The same with a read that breaks its task, which says nothing either.
#[tokio::test(start_paused = true)]
async fn a_read_of_the_store_that_breaks_cancels_nothing() {
    let rig = stored().await;
    let (caller, _job) = running(&rig, 1).await;
    leaves(caller).await;
    let store = rig.store.as_ref().expect("a store");
    store.reads_panic.store(true, Ordering::SeqCst);
    rig.until("the rows were read", || {
        store.panicked.load(Ordering::SeqCst) > 0
    })
    .await;
    pass(&rig, GRACE * 2).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    store.reads_panic.store(false, Ordering::SeqCst);
    rig.until("the job is cancelled", || rig.calls_of("CancelJob") == 1)
        .await;
}

/// The row of a caller on another frontend lives for `--executor-live-secs` after it was
/// written, and names one operation: not one whose id it begins.
#[tokio::test(start_paused = true)]
async fn a_watch_row_names_one_operation_and_expires_unless_renewed() {
    let rig = stored().await;
    let theirs = rig.rows("p-other");
    let reader = rig.rows("reader");
    theirs.watch(OperationId(1), rig.cfg.live).expect("written");
    assert_eq!(reader.watched(OperationId(1)), Ok(true));
    assert_eq!(reader.watched(OperationId(0x10)), Ok(false));
    assert_eq!(reader.watched(OperationId(0x100)), Ok(false));
    rig.advance(rig.cfg.live - Duration::from_millis(1)).await;
    assert_eq!(reader.watched(OperationId(1)), Ok(true));
    rig.advance(Duration::from_millis(1)).await;
    assert_eq!(reader.watched(OperationId(1)), Ok(false), "expired");
}

// ---- an operation taken up from a gone process

/// The row of an operation a gone process left, its job queued at the job service.
fn left_by_a_gone_process(rig: &Rig, operation: u64) -> String {
    let id = job_id(&rig.cfg.frontend, RunId::new(OperationId(operation), 0));
    rig.fake
        .record(JobSpec {
            job_id: id.clone(),
            image: IMAGE.to_owned(),
            wall_limit_seconds: Some(360),
            network: NetworkLevel::None.into(),
            constraints: Some(rig.cfg.constraints()),
            ..JobSpec::default()
        })
        .expect("a job");
    let action = digest_of(1);
    let row = OpRow {
        owner: String::new(),
        instance: "main".to_owned(),
        hash: action.hash_hex(),
        size: action.size_bytes,
        attempt: 0,
        job: id.clone(),
        key: Some(format!("cas/b/{operation}-0/aa")),
        whole_machine: false,
        tree: None,
    };
    rig.rows("gone")
        .put_op(OperationId(operation), &row)
        .expect("row");
    id
}

/// Catches an operation taken up with no caller that is never cancelled, or one cancelled
/// before a client had the grace to come back: its grace starts when it is taken up.
#[tokio::test(start_paused = true)]
async fn an_operation_taken_up_with_no_caller_is_cancelled_after_the_grace() {
    let rig = stored().await;
    let job = left_by_a_gone_process(&rig, 50);
    let running = Running::spawn(&rig.exec);
    running.kept(1).await;
    assert_eq!(rig.counted("kbf_executor_operations_reattached_total "), 1);
    rig.host_runs(&job);
    settle().await;
    assert_eq!(rig.counted(ALONE), 1);

    rig.advance(SHORT).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    rig.advance(Duration::from_millis(1)).await;
    assert_eq!(rig.calls_of("CancelJob"), 1);
    assert_eq!(job_state(&rig, &job), JobState::Cancelled);
    running.stop().await;
}

/// Catches a client that comes back by name to an operation taken up and is not counted: its
/// WaitExecution keeps the job, which it is answered from.
#[tokio::test(start_paused = true)]
async fn a_client_back_by_name_keeps_an_operation_taken_up() {
    let rig = stored().await;
    let job = left_by_a_gone_process(&rig, 51);
    let running = Running::spawn(&rig.exec);
    running.kept(1).await;
    let back = Caller::of(
        rig.exec
            .wait(&stored_name(OperationId(51)))
            .expect("open here"),
    );
    pass(&rig, GRACE * 2).await;
    assert_eq!(rig.calls_of("CancelJob"), 0);
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&back).await, Finished::Ran(_)));
    running.stop().await;
}
