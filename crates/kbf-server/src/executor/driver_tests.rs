//! The executor's task beside the frontend: liveness, the sweep of gone processes' operations,
//! and leaving.

use std::sync::atomic::Ordering;
use std::time::Duration;

use js_proto::v1::{JobSpec, JobState, NetworkLevel};
use kbf_front::{Dispatch, Finished};
use kbf_state::Store;
use kbf_types::OperationId;

use super::*;
use crate::executor::rows::OpRow;
use crate::executor::task::RunId;
use crate::executor::tests::{Rig, digest_of, settle};
use crate::executor::{job_id, stored_name};

const BEAT: Duration = Duration::from_secs(5);

fn row(tag: u8, job: &str, key: Option<&str>) -> OpRow {
    let action = digest_of(tag);
    OpRow {
        owner: String::new(),
        instance: "main".to_owned(),
        hash: action.hash_hex(),
        size: action.size_bytes,
        attempt: 0,
        job: job.to_owned(),
        key: key.map(str::to_owned),
        whole_machine: false,
    }
}

/// The job a gone process submitted for attempt `attempt` of `operation`.
fn gone_job(rig: &Rig, operation: u64, attempt: u32) -> String {
    let id = job_id(
        &rig.cfg.frontend,
        RunId::new(OperationId(operation), attempt),
    );
    rig.fake
        .record(JobSpec {
            job_id: id.clone(),
            image: crate::executor::tests::IMAGE.to_owned(),
            wall_limit_seconds: Some(360),
            network: NetworkLevel::None.into(),
            constraints: Some(rig.cfg.constraints()),
            ..JobSpec::default()
        })
        .expect("a job");
    id
}

fn live_row(rig: &Rig) -> bool {
    let store = rig.store.as_ref().expect("store");
    store.get("exec/live/p-main").expect("read").is_some()
}

/// An executor with no state store keeps nothing, and its task only waits for the stop.
#[tokio::test(start_paused = true)]
async fn an_executor_with_no_store_has_nothing_to_keep_and_stops() {
    let rig = Rig::start().await;
    let running = Running::spawn(&rig.exec);
    settle().await;
    rig.advance(Duration::from_secs(60)).await;
    running.stop().await;
}

/// The liveness row is written at once, renewed before it expires, and deleted on stop.
#[tokio::test(start_paused = true)]
async fn the_liveness_row_is_written_renewed_and_deleted_on_stop() {
    let rig = Rig::stored().await;
    let running = Running::spawn(&rig.exec);
    settle().await;
    assert!(live_row(&rig), "written at once");
    for _ in 0..6 {
        rig.advance(BEAT).await;
    }
    assert!(live_row(&rig), "30 s on a 20 s row: it was renewed");
    running.stop().await;
    assert!(
        !live_row(&rig),
        "deleted on stop, so the others take over at once"
    );
}

/// The sweep runs every half of the liveness time: an operation row of a gone process that
/// appears after the start is claimed at the next sweep, not at each beat; a row naming no
/// action is left claimed and opens nothing.
#[tokio::test(start_paused = true)]
async fn the_sweep_runs_every_half_of_the_liveness_time_and_skips_a_row_naming_no_action() {
    let rig = Rig::stored().await;
    let running = Running::spawn(&rig.exec);
    settle().await;
    let bad = OpRow {
        hash: "zz".to_owned(),
        ..row(1, "j", None)
    };
    let theirs = rig.rows("gone");
    theirs.put_op(OperationId(5), &bad).expect("row");
    let owner = || {
        rig.rows("reader")
            .op(OperationId(5))
            .expect("read")
            .expect("there")
            .owner
    };
    rig.advance(BEAT).await;
    assert_eq!(owner(), "gone", "not a sweep beat");
    rig.advance(BEAT).await;
    assert_eq!(owner(), "p-main", "claimed at the sweep");
    assert_eq!(rig.exec.open(), 0);
    assert_eq!(rig.counted("kbf_executor_operations_reattached_total "), 0);
    running.stop().await;
}

/// Catches a put-directory operation of a gone process that is not finished: its job is read
/// back where it is, its pack read from the row's put directory, its answer written.
#[tokio::test(start_paused = true)]
async fn an_operation_of_a_gone_process_is_taken_up_and_finished_from_its_put_directory() {
    let rig = Rig::stored().await;
    let job = gone_job(&rig, 50, 0);
    rig.rows("gone")
        .put_op(OperationId(50), &row(1, &job, Some("cas/b/50-0/aa")))
        .expect("row");
    let running = Running::spawn(&rig.exec);
    settle().await;
    assert_eq!(rig.counted("kbf_executor_operations_reattached_total "), 1);
    let ticket = rig
        .exec
        .wait(&stored_name(OperationId(50)))
        .expect("open here");
    let caller = crate::executor::tests::Caller::of(ticket);
    rig.host_finishes(&job, 0);
    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(_)));
    let ingested = rig.ingest.ingested.lock().unwrap().clone();
    assert_eq!(
        ingested,
        [(RunId::new(OperationId(50), 0), "cas/b/50-0/aa".to_owned())]
    );
    let rows = rig.rows("reader");
    assert_eq!(rows.op(OperationId(50)), Ok(None));
    assert!(rows.done(OperationId(50)).expect("read").is_some());
    running.stop().await;
}

/// Catches an operation of a gone process whose row names no put directory finished by another:
/// there is no pack to read, so its job is cancelled and the operation runs again as its next
/// attempt.
#[tokio::test(start_paused = true)]
async fn an_operation_with_no_put_directory_of_a_gone_process_is_cancelled_and_run_again() {
    let rig = Rig::stored().await;
    let old = gone_job(&rig, 60, 0);
    rig.rows("gone")
        .put_op(OperationId(60), &row(1, &old, None))
        .expect("row");
    let running = Running::spawn(&rig.exec);
    settle().await;
    let caller = crate::executor::tests::Caller::of(
        rig.exec
            .wait(&stored_name(OperationId(60)))
            .expect("open here"),
    );
    let next = job_id(&rig.cfg.frontend, RunId::new(OperationId(60), 1));
    rig.until("the next attempt is submitted", || {
        rig.fake.submitted(&next).is_some()
    })
    .await;
    let jobs = rig.fake.jobs();
    let first = jobs.iter().find(|j| j.id == old).expect("the old job");
    assert_eq!(first.state(), JobState::Cancelled);
    rig.host_finishes(&next, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 1);
    running.stop().await;
}

/// An operation this process already runs is not taken up a second time, when its row was
/// taken by another that is gone too.
#[tokio::test(start_paused = true)]
async fn an_operation_open_here_is_not_taken_up_again() {
    let rig = Rig::stored().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let thief = rig.rows("p-thief");
    let orphan = thief
        .orphans()
        .expect("scan")
        .into_iter()
        .next()
        .expect("this process's row");
    thief.claim(&orphan).expect("taken");
    let running = Running::spawn(&rig.exec);
    settle().await;
    assert_eq!(rig.counted("kbf_executor_operations_reattached_total "), 0);
    assert_eq!(rig.exec.open(), 1);
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    running.stop().await;
}

/// A store that fails does not stop the task: the failed renewal, the unreadable scan and the
/// refused claim are each tried again, and the task still stops.
#[tokio::test(start_paused = true)]
async fn failures_of_the_store_are_logged_and_tried_again_at_the_next_beat() {
    let rig = Rig::stored().await;
    let store = rig.store.as_ref().expect("store");
    // The first write is the first beat.
    store.fail_nth_write(1);
    let running = Running::spawn(&rig.exec);
    settle().await;
    assert!(!live_row(&rig), "the first renewal failed");
    rig.advance(BEAT).await;
    assert!(live_row(&rig), "the next one did not");
    running.stop().await;

    store.reads_fail.store(true, Ordering::SeqCst);
    let running = Running::spawn(&rig.exec);
    settle().await;
    store.reads_fail.store(false, Ordering::SeqCst);
    assert!(live_row(&rig), "the renewal needs no read");
    running.stop().await;

    rig.rows("gone")
        .put_op(OperationId(7), &row(1, "j", None))
        .expect("row");
    // The beat, then the claim.
    store.fail_nth_write(2);
    let running = Running::spawn(&rig.exec);
    settle().await;
    let owner = rig
        .rows("reader")
        .op(OperationId(7))
        .expect("read")
        .expect("there")
        .owner;
    assert_eq!(owner, "gone", "the claim failed");
    running.stop().await;
}

/// A store that panics ends neither the task nor its stop.
#[tokio::test(start_paused = true)]
async fn a_store_that_panics_does_not_end_the_task_or_its_stop() {
    let rig = Rig::stored().await;
    let store = rig.store.as_ref().expect("store");
    store.writes_panic.store(true, Ordering::SeqCst);
    let running = Running::spawn(&rig.exec);
    settle().await;
    rig.advance(BEAT).await;
    running.stop().await;
    store.writes_panic.store(false, Ordering::SeqCst);
    assert!(!live_row(&rig));
}

/// A liveness row that cannot be deleted at the stop is left to expire on its own.
#[tokio::test(start_paused = true)]
async fn a_liveness_row_that_cannot_be_deleted_expires_on_its_own() {
    let rig = Rig::stored().await;
    let running = Running::spawn(&rig.exec);
    settle().await;
    assert!(live_row(&rig));
    rig.store
        .as_ref()
        .expect("store")
        .writes_fail
        .store(true, Ordering::SeqCst);
    running.stop().await;
    rig.store
        .as_ref()
        .expect("store")
        .writes_fail
        .store(false, Ordering::SeqCst);
    assert!(live_row(&rig), "the delete failed");
    rig.advance(Duration::from_secs(21)).await;
    assert!(
        !live_row(&rig),
        "and the row expires after its time to live"
    );
}
