//! An operation over a state store: its row, its answer, and who runs it.

use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::{Dispatch, Finished, Stage};
use kbf_proto::reapi::ActionResult;
use kbf_state::{Cond, Store, Txn};
use kbf_types::ActionKey;
use tonic::Code;

use super::*;
use crate::executor::rows::{OpRow, op_key};
use crate::executor::tests::{Rig, digest_of};
use crate::executor::{OperationId, stored_name};

fn ran(exit_code: i32) -> Finished {
    Finished::Ran(Box::new(ActionResult {
        exit_code,
        ..ActionResult::default()
    }))
}

fn key_of(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: digest_of(tag),
    }
}

fn row_of(tag: u8, job: &str) -> OpRow {
    let key = key_of(tag);
    OpRow {
        owner: String::new(),
        instance: key.instance,
        hash: key.action.hash_hex(),
        size: key.action.size_bytes,
        attempt: 0,
        job: job.to_owned(),
        key: None,
        whole_machine: false,
    }
}

fn exit_code(finished: &Finished) -> i32 {
    match finished {
        Finished::Ran(result) => result.exit_code,
        other => panic!("not a result: {other:?}"),
    }
}

/// The row of an operation is there from before its submit to its finish, naming its job and
/// owner, and its write is timed as the `row` step; its answer and action are there after.
#[tokio::test(start_paused = true)]
async fn an_operation_keeps_its_row_from_before_its_submit_until_it_finishes() {
    let rig = Rig::stored().await;
    let reader = rig.rows("reader");
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let row = reader
        .op(caller.operation())
        .expect("read")
        .expect("the row is there");
    let key = row.key.clone().expect("a put directory");
    assert!(key.starts_with("cas/b/"), "{key}");
    assert_eq!(
        row,
        OpRow {
            owner: "p-main".to_owned(),
            attempt: 0,
            job: job.clone(),
            key: Some(key),
            whole_machine: false,
            ..row_of(1, "")
        }
    );
    assert_eq!(
        rig.counted("kbf_executor_step_seconds_count{step=\"row\"} "),
        1,
        "the row's write is timed"
    );
    rig.host_finishes(&job, 0);
    let finished = rig.until_done(&caller).await;
    assert_eq!(reader.op(caller.operation()), Ok(None));
    assert_eq!(
        reader.done(caller.operation()),
        Ok(Some((key_of(1), finished)))
    );
}

/// A run's row names its put directory, which another process finishes it from.
#[tokio::test(start_paused = true)]
async fn the_row_of_a_run_names_its_put_directory() {
    let rig = Rig::stored().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let row = rig
        .rows("reader")
        .op(caller.operation())
        .expect("read")
        .expect("there");
    let key = row.key.expect("a put directory");
    assert!(
        key.starts_with(&format!("cas/b/{}-0/", caller.operation().0)),
        "{key}"
    );
    rig.host_finishes(&job, 0);
    rig.until_done(&caller).await;
}

/// Catches a run that goes on where another process owns the row: the Execute is handed over,
/// no job is submitted, and the callers follow the answer the owner writes.
#[tokio::test(start_paused = true)]
async fn an_operation_another_process_owns_is_handed_over_and_its_callers_follow() {
    let rig = Rig::stored().await;
    let theirs = rig.rows("p-theirs");
    theirs
        .put_op(OperationId(1), &row_of(1, "their-job"))
        .expect("their row");
    let caller = rig.submit(1);
    assert_eq!(
        caller.operation(),
        OperationId(1),
        "the first id of the store"
    );
    rig.until("handed over", || rig.exec.open() == 0).await;
    assert!(rig.fake.calls().is_empty(), "no job of this process");
    assert!(caller.finished().is_none());
    theirs
        .finish(
            OperationId(1),
            &key_of(1),
            &ran(9),
            Duration::from_secs(600),
        )
        .expect("their answer");
    let finished = rig.until_done(&caller).await;
    assert_eq!(exit_code(&finished), 9);
    assert_eq!(
        rig.exec.wait(&caller.name).map(|t| t.name),
        Some(caller.name.clone())
    );
}

/// A row that cannot be written does not stop the run: it is only that no other frontend could
/// take it up.
#[tokio::test(start_paused = true)]
async fn a_row_that_cannot_be_written_does_not_stop_the_run() {
    let rig = Rig::stored().await;
    // The first write is the id block, the second this operation's row.
    rig.store.as_ref().expect("store").fail_nth_write(2);
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    assert_eq!(
        rig.rows("reader").op(caller.operation()),
        Ok(None),
        "the row was not written"
    );
    rig.host_finishes(&job, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// An answer the store refuses still answers the callers of this process at once, and is
/// written again until the store takes it, so other frontends answer it by name too.
#[tokio::test(start_paused = true)]
async fn an_answer_that_cannot_be_written_answers_the_callers_here_and_is_written_again() {
    let rig = Rig::stored().await;
    // The id block, the row, and then the answer.
    rig.store.as_ref().expect("store").fail_nth_write(3);
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_finishes(&job, 0);
    let answer = rig.until_done(&caller).await;
    assert!(matches!(answer, Finished::Ran(_)));
    assert!(rig.exec.wait(&caller.name).is_some(), "kept in memory");
    rig.until("the answer is written again", || {
        rig.rows("reader")
            .done(caller.operation())
            .is_ok_and(|done| done.is_some())
    })
    .await;
    let written = rig.rows("reader").done(caller.operation());
    assert_eq!(
        written.expect("a read").map(|(_, answer)| answer),
        Some(answer)
    );
}

/// Catches two writers of one answer: when another process wrote it first, that answer is
/// the callers'.
#[tokio::test(start_paused = true)]
async fn an_answer_another_frontend_wrote_first_is_the_one_the_callers_get() {
    let rig = Rig::stored().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let operation = caller.operation();
    let store = rig.store.as_ref().expect("store");
    let mut of = vec![1_u8];
    of.extend(key_of(1).action.size_bytes.to_be_bytes());
    of.extend(key_of(1).action.hash);
    of.extend(b"main");
    let at = format!("{:016x}", operation.0);
    store
        .txn(
            Txn::new()
                .put(format!("done/{at}"), ran(77).to_row(), Cond::Any)
                .put(format!("doneof/{at}"), of, Cond::Any),
        )
        .expect("a write")
        .expect("no conflict");
    rig.host_finishes(&job, 0);
    let finished = rig.until_done(&caller).await;
    assert_eq!(exit_code(&finished), 77);
    assert_eq!(rig.rows("reader").op(operation), Ok(None), "its row goes");
}

/// Catches a run that finishes an operation whose row another process holds: it stops, and its
/// callers follow the answer the owner writes (not the one this process read from the pack).
#[tokio::test(start_paused = true)]
async fn an_operation_whose_row_was_taken_up_meanwhile_is_handed_over_at_its_finish() {
    let rig = Rig::stored().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    let operation = caller.operation();
    let thief = rig.rows("p-thief");
    let orphan = thief
        .orphans()
        .expect("scan")
        .into_iter()
        .find(|o| o.operation == operation)
        .expect("this process has no liveness row: its row is an orphan's");
    thief.claim(&orphan).expect("the thief takes it up");
    rig.host_finishes(&job, 0);
    rig.until("handed over", || rig.exec.open() == 0).await;
    assert!(
        caller.finished().is_none(),
        "this process's answer is not given"
    );
    thief
        .finish(operation, &key_of(1), &ran(5), Duration::from_secs(600))
        .expect("the thief's answer");
    assert_eq!(exit_code(&rig.until_done(&caller).await), 5);
}

/// A follower keeps asking while the store cannot be read, and is answered UNAVAILABLE when
/// the read panics: its callers are not left waiting.
#[tokio::test(start_paused = true)]
async fn a_follower_asks_again_while_the_store_fails_and_is_answered_if_its_read_panics() {
    let rig = Rig::stored().await;
    let theirs = rig.rows("p-theirs");
    for n in [20, 21] {
        theirs.put_op(OperationId(n), &row_of(1, "j")).expect("row");
    }
    let a = rig
        .exec
        .wait(&stored_name(OperationId(20)))
        .expect("followed");
    let store = rig.store.as_ref().expect("store");
    store.reads_fail.store(true, Ordering::SeqCst);
    rig.advance(rig.cfg.poll).await;
    rig.advance(rig.cfg.poll).await;
    assert_eq!(*a.stage.borrow(), Stage::Queued);
    store.reads_fail.store(false, Ordering::SeqCst);
    theirs
        .finish(
            OperationId(20),
            &key_of(1),
            &ran(1),
            Duration::from_secs(600),
        )
        .expect("answer");
    rig.advance(rig.cfg.poll).await;
    assert!(matches!(&*a.stage.borrow(), Stage::Done(_)));
    let b = rig
        .exec
        .wait(&stored_name(OperationId(21)))
        .expect("followed");
    crate::executor::tests::settle().await;
    store.reads_panic.store(true, Ordering::SeqCst);
    rig.advance(rig.cfg.poll).await;
    store.reads_panic.store(false, Ordering::SeqCst);
    theirs
        .finish(
            OperationId(21),
            &key_of(1),
            &ran(2),
            Duration::from_secs(600),
        )
        .expect("answer");
    rig.advance(rig.cfg.poll).await;
    let Stage::Done(Finished::Failed(status)) = &*b.stage.borrow() else {
        panic!("a follower whose read broke is answered");
    };
    assert_eq!(status.code, tonic::Code::Unavailable as i32);
}

/// A follower whose operation is no longer followed ends at its next look, answering no one.
#[tokio::test(start_paused = true)]
async fn a_follower_task_of_an_operation_no_one_follows_ends_at_its_next_look() {
    let rig = Rig::stored().await;
    let task = tokio::spawn(follow_task(Arc::clone(rig.exec.inner()), OperationId(5)));
    crate::executor::tests::settle().await;
    rig.advance(rig.cfg.poll).await;
    task.await.expect("it ended");
    assert_eq!(rig.exec.open(), 0);
    let _ = (op_key(OperationId(5)), Code::Ok);
}

/// Catches two callers that ask together for an operation another frontend runs each starting a
/// watch of the store (two reads every poll, two answers sent): the second is given the first's
/// follower, and both see the one answer.
#[tokio::test(start_paused = true)]
async fn two_followers_of_one_operation_share_one_watch() {
    let rig = Rig::stored().await;
    let theirs = rig.rows("p-other");
    let operation = OperationId(90);
    theirs
        .put_op(operation, &row_of(1, "job-90"))
        .expect("their row");
    let inner = rig.exec.inner();
    let first = follow(inner, operation, key_of(1));
    let second = follow(inner, operation, key_of(1));
    assert!(inner.ops().is_followed(operation));
    theirs
        .finish(operation, &key_of(1), &ran(4), Duration::from_secs(600))
        .expect("their answer");
    rig.until("both followers are answered", || {
        matches!(*first.borrow(), Stage::Done(_)) && matches!(*second.borrow(), Stage::Done(_))
    })
    .await;
    for stage in [first, second] {
        assert_eq!(*stage.borrow(), Stage::Done(ran(4)));
    }
    assert!(!inner.ops().is_followed(operation));
}
