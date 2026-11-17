//! The dispatcher: what an Execute, a wait by name and a cancel do, on the rig of `mod_tests`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::{Dispatch, Finished, Stage};
use kbf_proto::reapi::ActionResult;
use kbf_state::{Store, Txn};
use kbf_types::{ActionKey, OperationId};

use super::*;
use crate::executor::tests::{Caller, FakeIngest, Rig, digest_n, digest_of, settle, submission_of};
use crate::executor::{rows, stored_name};

// ---- an Execute is a job, at once

/// Catches an Execute that waits for a poll tick, a lease or a slot before its job exists: the
/// clock is paused and never advanced, and the job is at the service.
#[tokio::test(start_paused = true)]
async fn an_execute_is_submitted_before_the_clock_moves() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    settle().await;
    let job = rig.job_of(&caller, 0);
    let spec = rig
        .fake
        .submitted(&job)
        .expect("the job is at the job service with no poll tick");
    assert_eq!(spec.job_id, job);
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(caller.seen(), Stage::Queued);
    assert_eq!(rig.exec.open(), 1);
    assert_eq!(rig.counted("kbf_executor_jobs_submitted_total "), 1);
    let labels = &spec.labels;
    assert_eq!(
        labels.get("kbf-frontend").map(String::as_str),
        Some("front-1")
    );
    assert_eq!(
        labels.get("kbf-instance").map(String::as_str),
        Some("p-main")
    );
}

/// Catches a cap on the jobs a frontend has in flight: every one of many distinct Executes is
/// at the job service before any job has ended.
#[tokio::test(start_paused = true)]
async fn no_cap_holds_back_concurrent_executes() {
    let rig = Rig::start().await;
    let callers: Vec<Caller> = (0..60)
        .map(|n| Caller::of(rig.exec.submit(submission_of(digest_n(n))).expect("taken")))
        .collect();
    rig.until("every job is submitted", || rig.fake.jobs().len() == 60)
        .await;
    assert!(callers.iter().all(|caller| caller.finished().is_none()));
    assert_eq!(rig.exec.open(), 60);
    assert_eq!(rig.counted("kbf_executor_jobs_submitted_total "), 60);
}

/// An action that ran is answered with the result the ingest rebuilt, the callers see it
/// executing once its job runs, and the operation is closed.
#[tokio::test(start_paused = true)]
async fn an_action_that_ran_answers_its_callers_with_its_result() {
    let rig = Rig::start().await;
    rig.ingest
        .script
        .lock()
        .unwrap()
        .push_back(FakeIngest::ran(7));
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    rig.until("the callers see it executing", || {
        caller.seen() == Stage::Executing
    })
    .await;
    assert!(rig.fake.finish(&job, 0));
    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(result) if result.exit_code == 7));
    assert_eq!(rig.exec.open(), 0);
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"ran\"} "), 1);
    assert_eq!(rig.counted("kbf_executor_ingests_total{result=\"ok\"} "), 1);
    let ingested = rig.ingest.ingested.lock().unwrap().clone();
    assert_eq!(ingested.len(), 1);
    assert_eq!(ingested[0].0, RunId::new(caller.operation(), 0));
}

// ---- joining

/// Two Executes of one action are one operation and one job, and both callers are answered;
/// an Execute of the action after it finished is a new operation.
#[tokio::test(start_paused = true)]
async fn two_executes_of_one_action_share_one_job_and_both_are_answered() {
    let rig = Rig::start().await;
    let (first, second) = (rig.submit(5), rig.submit(5));
    assert_eq!(first.name, second.name);
    assert_eq!(rig.exec.open(), 1);
    let job = rig.submitted(&first, 0).await;
    rig.host_finishes(&job, 0);
    let (a, b) = (rig.until_done(&first).await, rig.until_done(&second).await);
    assert_eq!(a, b);
    assert_eq!(rig.fake.jobs().len(), 1);
    let third = rig.submit(5);
    assert_ne!(third.name, first.name, "a finished operation is not joined");
}

/// Executes of different actions, or of one action on different instances, are different
/// operations.
#[tokio::test(start_paused = true)]
async fn executes_of_different_actions_or_instances_are_different_operations() {
    let rig = Rig::start().await;
    let one = rig.submit(1);
    let two = rig.submit(2);
    let mut other = submission_of(digest_of(1));
    other.key.instance = "elsewhere".to_owned();
    let three = Caller::of(rig.exec.submit(other).expect("taken"));
    let names = [&one.name, &two.name, &three.name];
    assert_eq!(
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
}

// ---- by name

/// A name asks for an open operation, and for one finished within the retention; after the
/// retention a finished operation is no longer known.
#[tokio::test(start_paused = true)]
async fn wait_finds_an_open_operation_and_a_finished_one_until_its_retention_ends() {
    let rig = Rig::with(|cfg| cfg.finished_retention = Duration::from_secs(30)).await;
    let caller = rig.submit(1);
    let open = rig
        .exec
        .wait(&caller.name)
        .expect("an open operation is known");
    assert_eq!(open.name, caller.name);
    assert_eq!(open.instance, "main");
    assert_eq!(open.action, digest_of(1));
    let job = rig.submitted(&caller, 0).await;
    rig.host_finishes(&job, 0);
    let finished = rig.until_done(&caller).await;
    let kept = rig.exec.wait(&caller.name).expect("kept for its retention");
    assert_eq!(*kept.stage.borrow(), Stage::Done(finished));
    rig.advance(Duration::from_secs(29)).await;
    assert!(rig.exec.wait(&caller.name).is_some());
    rig.advance(Duration::from_secs(2)).await;
    assert!(
        rig.exec.wait(&caller.name).is_none(),
        "pruned after its retention"
    );
}

/// A name that is no operation of this executor is unknown, whatever its spelling.
#[tokio::test(start_paused = true)]
async fn wait_and_cancel_refuse_names_that_name_no_operation() {
    let rig = Rig::start().await;
    let real = rig.submit(1);
    let id = real.operation().0;
    for name in [
        "",
        "operations/",
        "operations/cached/abc",
        "operations/-1",
        "operations/+1",
        "operations/01",
        "operations/18446744073709551616",
        "operations/12x",
        "jobs/1",
        &format!("operations/{}", id + 1),
    ] {
        assert!(rig.exec.wait(name).is_none(), "{name:?}");
        assert!(!rig.exec.cancel(name), "{name:?}");
    }
    assert!(rig.exec.wait(&real.name).is_some());
}

/// The names are `operations/<decimal id>`, one spelling each.
#[test]
fn stored_name_and_parse_stored_name_round_trip_and_refuse_other_spellings() {
    for id in [0, 1, 9, 10, 1024, u64::MAX] {
        let name = stored_name(OperationId(id));
        assert_eq!(name, format!("operations/{id}"));
        assert_eq!(parse_stored_name(&name), Some(OperationId(id)));
    }
    for bad in [
        "operations/00",
        "operations/007",
        "operations/ 1",
        "operations/1 ",
        "Operations/1",
    ] {
        assert_eq!(parse_stored_name(bad), None, "{bad:?}");
    }
    assert_eq!(parse_stored_name("operations/0"), Some(OperationId(0)));
}

// ---- the bound, the refusals and the odds and ends

/// The one bound is the memory guard: an Execute past the open operations it allows is refused
/// UNAVAILABLE, while one that joins an open operation still is taken.
#[tokio::test(start_paused = true)]
async fn an_execute_past_the_open_limit_is_refused_and_a_twin_still_joins() {
    let rig = Rig::start().await;
    rig.inputs.hang.store(true, Ordering::SeqCst);
    let mut first = None;
    for n in 0..MAX_OPEN {
        let n = u32::try_from(n).expect("a small number");
        let ticket = rig
            .exec
            .submit(submission_of(digest_n(n)))
            .expect("under the limit");
        first.get_or_insert(ticket.name);
    }
    assert_eq!(rig.exec.open(), MAX_OPEN);
    let refused = rig
        .exec
        .submit(submission_of(digest_n(u32::MAX)))
        .expect_err("past the limit");
    assert_eq!(refused.code(), tonic::Code::Unavailable);
    assert!(refused.message().contains("65536"), "{}", refused.message());
    let joined = rig
        .exec
        .submit(submission_of(digest_n(0)))
        .expect("a twin joins at the limit");
    assert_eq!(Some(joined.name), first);
    assert_eq!(rig.exec.open(), MAX_OPEN);
}

/// A server with no executor refuses an Execute FAILED_PRECONDITION and knows no name.
#[tokio::test(start_paused = true)]
async fn a_dispatcher_with_no_executor_refuses_every_execute() {
    let none = Dispatcher::of(None);
    let refused = none
        .submit(submission_of(digest_of(1)))
        .expect_err("refused");
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(refused.message().contains("no executor"));
    assert!(none.wait("operations/1").is_none());
    assert!(!none.cancel("operations/1"));
}

/// A dispatcher over an executor hands it the Execute, the name and the cancel.
#[tokio::test(start_paused = true)]
async fn a_dispatcher_over_an_executor_routes_to_it() {
    let rig = Rig::start().await;
    let dispatcher = Dispatcher::of(Some(rig.exec.clone()));
    let caller = Caller::of(
        dispatcher
            .submit(submission_of(digest_of(3)))
            .expect("taken"),
    );
    assert!(dispatcher.wait(&caller.name).is_some());
    assert!(dispatcher.cancel(&caller.name));
    assert!(matches!(
        rig.until_done(&caller).await,
        Finished::Failed(status) if status.code == tonic::Code::Cancelled as i32
    ));
}

/// A config the rules refuse is no executor.
#[tokio::test(start_paused = true)]
async fn an_executor_is_refused_a_config_that_breaks_a_rule() {
    let rig = Rig::start().await;
    let mut cfg = rig.cfg.clone();
    cfg.frontend = String::new();
    assert_eq!(
        Executor::new(cfg, rig.deps()).expect_err("refused"),
        ConfigError::Frontend
    );
}

/// An executor with no state store numbers its operations one after another, from a base no
/// earlier process of the same name shares.
#[tokio::test(start_paused = true)]
async fn operations_of_a_process_with_no_store_are_numbered_from_its_start_time() {
    let rig = Rig::start().await;
    let (a, b) = (rig.submit(1), rig.submit(2));
    assert_eq!(a.operation().0 + 1, b.operation().0);
    assert!(
        a.operation().0 >= 1 << 16,
        "the base is a time shifted by 16 bits"
    );
    assert!(format!("{:?}", rig.exec).contains("front-1"));
}

/// The job service as `/readyz` asks: it answers a read of a job no one made, or it does not.
#[tokio::test(start_paused = true)]
async fn the_probe_passes_when_the_job_service_answers_and_fails_when_it_will_not_serve() {
    use crate::health::Probe;
    let rig = Rig::start().await;
    let probe = rig.exec.probe(Duration::from_secs(5));
    assert_eq!(probe.probe().await, Ok(()));
    rig.fake.inject(kbf_job_client::fake::Fault::Status {
        code: tonic::Code::Unauthenticated,
        message: "no".to_owned(),
    });
    let failed = probe.probe().await.expect_err("a refused token");
    assert!(failed.contains("did not answer"), "{failed}");
}

// ---- over a state store

/// Another frontend of the store answers by name an operation that this one finished, from its
/// `done/` row, and knows no name it never gave.
#[tokio::test(start_paused = true)]
async fn an_operation_finished_elsewhere_is_answered_from_the_done_row() {
    let rig = Rig::stored().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_finishes(&job, 0);
    let finished = rig.until_done(&caller).await;
    let other = rig.executor("p-other");
    let ticket = other.wait(&caller.name).expect("answered from the store");
    assert_eq!(*ticket.stage.borrow(), Stage::Done(finished));
    assert_eq!(ticket.instance, "main");
    assert_eq!(ticket.action, digest_of(1));
    assert!(other.wait("operations/999999").is_none());
}

/// A name another frontend runs is followed: its caller sees the answer once that frontend
/// writes it, and an operation whose row and answer are both gone is answered UNAVAILABLE.
#[tokio::test(start_paused = true)]
async fn an_operation_another_frontend_runs_is_followed_until_its_answer_is_written() {
    let rig = Rig::stored().await;
    let theirs = rig.rows("p-other");
    let key = ActionKey {
        instance: "main".to_owned(),
        action: digest_of(1),
    };
    let row = |op: u64| rows::OpRow {
        owner: String::new(),
        instance: "main".to_owned(),
        hash: key.action.hash_hex(),
        size: key.action.size_bytes,
        attempt: 0,
        job: format!("job-{op}"),
        key: None,
        whole_machine: false,
    };
    let (running, vanished) = (OperationId(70), OperationId(71));
    theirs.put_op(running, &row(70)).expect("their row");
    theirs.put_op(vanished, &row(71)).expect("their row");
    let a = Caller::of(rig.exec.wait(&stored_name(running)).expect("followed"));
    let b = Caller::of(rig.exec.wait(&stored_name(vanished)).expect("followed"));
    let again = rig
        .exec
        .wait(&stored_name(running))
        .expect("the same follower");
    assert_eq!(a.seen(), Stage::Queued);
    assert_eq!(*again.stage.borrow(), Stage::Queued);
    settle().await;
    rig.advance(rig.cfg.poll).await;
    assert!(a.finished().is_none(), "their row is still there");
    // Their answer is written: their row goes, and the answer and action come.
    let answer = Finished::Ran(Box::new(ActionResult {
        exit_code: 3,
        ..ActionResult::default()
    }));
    theirs
        .finish(running, &key, &answer, Duration::from_secs(600))
        .expect("their answer");
    // The other row and its answer are gone, as when its retention has passed.
    let store = rig.store.as_ref().expect("a store");
    store
        .txn(Txn::new().delete(rows::op_key(vanished), kbf_state::Cond::Any))
        .expect("store")
        .expect("deleted");
    rig.until("both are answered", || {
        a.finished().is_some() && b.finished().is_some()
    })
    .await;
    assert_eq!(a.finished(), Some(answer));
    assert!(matches!(
        b.finished(),
        Some(Finished::Failed(status)) if status.code == tonic::Code::Unavailable as i32
    ));
}

/// A row the state store cannot read or does not understand answers no name, and a name asked
/// of an executor with no store is only what memory holds.
#[tokio::test(start_paused = true)]
async fn a_name_the_store_cannot_read_is_unknown() {
    let rig = Rig::stored().await;
    let store = rig.store.as_ref().expect("a store");
    store
        .txn(Txn::new().put(
            rows::op_key(OperationId(80)),
            b"\x01not json".to_vec(),
            kbf_state::Cond::Any,
        ))
        .expect("store")
        .expect("put");
    assert!(rig.exec.wait("operations/80").is_none(), "a corrupt row");
    let bad_hash = rows::OpRow {
        owner: "p-other".to_owned(),
        instance: "main".to_owned(),
        hash: "xyz".to_owned(),
        size: 1,
        attempt: 0,
        job: "j".to_owned(),
        key: None,
        whole_machine: false,
    };
    let mut value = vec![rows::VERSION];
    value.extend(serde_json::to_vec(&bad_hash).expect("json"));
    store
        .txn(Txn::new().put(rows::op_key(OperationId(81)), value, kbf_state::Cond::Any))
        .expect("store")
        .expect("put");
    assert!(
        rig.exec.wait("operations/81").is_none(),
        "a row naming no action"
    );
    store.reads_fail.store(true, Ordering::SeqCst);
    assert!(
        rig.exec.wait("operations/82").is_none(),
        "a store that cannot be read"
    );
    let plain = Rig::start().await;
    assert!(plain.exec.wait("operations/82").is_none());
}

/// A store that cannot give an operation id refuses the Execute UNAVAILABLE, and opens no
/// operation.
#[tokio::test(start_paused = true)]
async fn a_store_that_gives_no_operation_id_refuses_the_execute() {
    let rig = Rig::stored().await;
    rig.store
        .as_ref()
        .expect("a store")
        .reads_fail
        .store(true, Ordering::SeqCst);
    let refused = rig
        .exec
        .submit(submission_of(digest_of(1)))
        .expect_err("no id");
    assert_eq!(refused.code(), tonic::Code::Unavailable);
    assert!(
        refused.message().contains("no operation id"),
        "{}",
        refused.message()
    );
    assert_eq!(rig.exec.open(), 0);
}

/// Two processes of one store never give one id, and never 0.
#[tokio::test(start_paused = true)]
async fn operation_ids_of_two_processes_over_one_store_never_meet() {
    let rig = Rig::stored().await;
    let other = rig.executor("p-other");
    let mut ids = std::collections::BTreeSet::new();
    for n in 0..6 {
        let one = rig.exec.submit(submission_of(digest_n(n))).expect("taken");
        let two = other
            .submit(submission_of(digest_n(100 + n)))
            .expect("taken");
        ids.insert(parse_stored_name(&one.name).expect("a name").0);
        ids.insert(parse_stored_name(&two.name).expect("a name").0);
    }
    assert_eq!(ids.len(), 12);
    assert!(!ids.contains(&0));
}
