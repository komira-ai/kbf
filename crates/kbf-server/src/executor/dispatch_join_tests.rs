//! One job per action across the frontends of a store: an Execute of an action another
//! frontend runs follows that operation instead of submitting a second job, on the rig of
//! `mod_tests` with two executors over one state store and one job service.

use kbf_front::{Dispatch, Finished};
use tonic::Code;

use super::*;
use crate::executor::tests::{Caller, FakeIngest, Rig, digest_of, settle, submission_of};

/// The caller of an Execute of the action of `tag` on `exec`.
fn submit(exec: &Executor, tag: u8) -> Caller {
    Caller::of(exec.submit(submission_of(digest_of(tag))).expect("taken"))
}

fn code(finished: &Finished) -> Option<i32> {
    match finished {
        Finished::Failed(status) => Some(status.code),
        _ => None,
    }
}

/// Catches a rebuild across frontends: an Execute on a second frontend of an action the first
/// runs submits no job of its own, follows the first operation, and is answered with its
/// result under its own name, which any frontend then answers from the store. One job ran,
/// and its pack was read once.
#[tokio::test(start_paused = true)]
async fn an_execute_of_an_action_another_frontend_runs_follows_it_with_no_second_job() {
    let rig = Rig::stored().await;
    rig.ingest
        .script
        .lock()
        .unwrap()
        .push_back(FakeIngest::ran(7));
    let first = rig.submit(5);
    let job = rig.submitted(&first, 0).await;
    let other = rig.executor("p-other");
    let second = submit(&other, 5);
    assert_ne!(second.name, first.name);
    rig.advance(rig.cfg.poll).await;
    assert_eq!(rig.fake.jobs().len(), 1, "a second job");
    assert_eq!(rig.calls_of("SubmitJob"), 1);
    assert!(second.finished().is_none());

    rig.host_finishes(&job, 0);
    let answer = rig.until_done(&first).await;
    assert!(matches!(&answer, Finished::Ran(result) if result.exit_code == 7));
    for _ in 0..4 {
        if second.finished().is_some() {
            break;
        }
        rig.advance(rig.cfg.poll).await;
    }
    assert_eq!(second.finished(), Some(answer.clone()));
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(
        rig.ingest.ingested.lock().unwrap().len(),
        1,
        "one pack read"
    );
    let third = rig.executor("p-third");
    let named = third.wait(&second.name).expect("answered from the store");
    assert_eq!(*named.stage.borrow(), kbf_front::Stage::Done(answer));
    assert_eq!(other.open(), 0);
}

/// Catches a follower answered CANCELLED for a cancel it never asked for: when the operation
/// it follows is cancelled by its own callers, the follower runs the action itself, as the
/// only open operation of it, with a job of its own.
#[tokio::test(start_paused = true)]
async fn a_follower_of_an_operation_its_callers_cancelled_runs_the_action_itself() {
    let rig = Rig::stored().await;
    let first = rig.submit(5);
    rig.submitted(&first, 0).await;
    let other = rig.executor("p-other");
    let second = submit(&other, 5);
    rig.advance(rig.cfg.poll).await;
    assert_eq!(rig.fake.jobs().len(), 1);

    assert!(rig.exec.cancel(&first.name));
    let cancelled = rig.until_done(&first).await;
    assert_eq!(code(&cancelled), Some(Code::Cancelled as i32));
    let own = rig.job_of(&second, 0);
    for _ in 0..4 {
        if rig.fake.submitted(&own).is_some() {
            break;
        }
        rig.advance(rig.cfg.poll).await;
    }
    assert!(rig.fake.submitted(&own).is_some(), "the follower's own job");
    assert!(second.finished().is_none(), "answered the other's cancel");
    rig.host_finishes(&own, 0);
    rig.until_done(&second).await;
}

/// Catches a cancel of a follower that reaches the operation it follows: the follower is
/// answered CANCELLED and the other frontend's operation and job run on for their callers.
#[tokio::test(start_paused = true)]
async fn a_cancelled_follower_leaves_the_operation_it_followed_running() {
    let rig = Rig::stored().await;
    let first = rig.submit(5);
    rig.submitted(&first, 0).await;
    let other = rig.executor("p-other");
    let second = submit(&other, 5);
    settle().await;
    rig.advance(rig.cfg.poll).await;
    assert!(other.cancel(&second.name));
    let answer = rig.until_done(&second).await;
    assert_eq!(code(&answer), Some(Code::Cancelled as i32));
    rig.advance(rig.cfg.poll).await;
    assert!(first.finished().is_none(), "the followed operation ended");
    assert_eq!(rig.calls_of("CancelJob"), 0);
    assert_eq!(rig.exec.open(), 1);
}

/// Catches an action row that outlives its operation when the store refuses the answer: the
/// callers are answered at once, the answer is written again every poll until the store takes
/// it, and with it the action's row goes, so a later Execute of the action on another frontend
/// runs a job of its own instead of following an operation no one will answer in the store.
#[tokio::test(start_paused = true)]
async fn an_answer_the_store_refused_is_written_again_and_frees_the_action() {
    use std::sync::atomic::Ordering;

    use kbf_state::Store as _;

    let rig = Rig::stored().await;
    let store = rig.store.clone().expect("a store");
    let action = digest_of(5);
    let act = rows::act_key("main", &action.hash_hex(), action.size_bytes);
    let first = rig.submit(5);
    let job = rig.submitted(&first, 0).await;
    assert!(store.get(&act).expect("a read").is_some(), "the action row");

    store.writes_fail.store(true, Ordering::SeqCst);
    rig.host_finishes(&job, 0);
    let answer = rig.until_done(&first).await;
    rig.advance(rig.cfg.poll).await;
    assert!(
        store.get(&act).expect("a read").is_some(),
        "written while refused"
    );
    store.writes_fail.store(false, Ordering::SeqCst);
    for _ in 0..4 {
        if store.get(&act).expect("a read").is_none() {
            break;
        }
        rig.advance(rig.cfg.poll).await;
    }
    assert!(
        store.get(&act).expect("a read").is_none(),
        "the action row stayed"
    );
    let other = rig.executor("p-other");
    let named = other.wait(&first.name).expect("answered from the store");
    assert_eq!(*named.stage.borrow(), kbf_front::Stage::Done(answer));

    let second = submit(&other, 5);
    let own = rig.job_of(&second, 0);
    for _ in 0..4 {
        if rig.fake.submitted(&own).is_some() {
            break;
        }
        rig.advance(rig.cfg.poll).await;
    }
    assert!(
        rig.fake.submitted(&own).is_some(),
        "the next Execute followed"
    );
}

/// Catches a follower that runs the action again, or answers otherwise, when the operation it
/// follows fails for a reason other than a cancel: the follower's callers get that failure, and
/// no job of its own is submitted.
#[tokio::test(start_paused = true)]
async fn a_follower_is_answered_with_the_failure_of_the_operation_it_follows() {
    let rig = Rig::stored().await;
    let theirs = rig.rows("p-other");
    let action = digest_of(5);
    let key = kbf_types::ActionKey {
        instance: "main".to_owned(),
        action,
    };
    let row = rows::OpRow {
        owner: String::new(),
        instance: "main".to_owned(),
        hash: action.hash_hex(),
        size: action.size_bytes,
        attempt: 0,
        job: "theirs".to_owned(),
        key: Some("cas/b/1/aa".to_owned()),
        whole_machine: false,
    };
    let other = kbf_types::OperationId(900_000);
    assert_eq!(theirs.open_op(other, &row), Ok(rows::Opened::Ours));
    let follower = rig.submit(5);
    rig.advance(rig.cfg.poll).await;
    assert!(follower.finished().is_none());

    let failed = Finished::Failed(kbf_proto::google::rpc::Status {
        code: Code::Internal as i32,
        message: "the farm could not run it".to_owned(),
        details: Vec::new(),
    });
    theirs
        .finish(other, &key, &failed, std::time::Duration::from_secs(600))
        .expect("their answer");
    let answer = rig.until_done(&follower).await;
    assert_eq!(answer, failed);
    assert_eq!(rig.fake.jobs().len(), 0, "a job of the follower's own");
}
