//! Tests of the farm's calls on a core (`dispatch`): what a refused submit is told, the
//! acknowledgement of each kind of `Result`, the tickets of a name a client asks for, and
//! the calls an admission refuses.

use std::sync::Arc;

use js_server_core::Beat;
use kbf_front::{Cache, Stage, Submission};
use kbf_proto::worker::{self, ResultAckReason, server_message};
use kbf_sched::Request;
use kbf_state::{Cond, Store as _, Txn};
use kbf_types::{ActionKey, FarmTime, LeaseId, OperationId, WorkKey, WorkerId};
use tonic::Code;

use super::dispatch;
use super::testing::{
    Flaky, denying, failing_cache, ran, register, request, rig, rig_caching, rig_over, rig_started,
    started, starts, submission, submit, wall,
};
use crate::action_usage::Shapes;

fn at(millis: u64) -> FarmTime {
    FarmTime::from_millis(millis)
}

/// The `Result` ack of `result` from `worker`, as the core's `report` gives it.
async fn report_of(
    rig: &super::testing::Rig,
    worker: &str,
    result: worker::Result,
) -> Option<worker::ResultAck> {
    let backend = &rig.backend;
    let report = dispatch::report(
        &rig.core,
        backend,
        &wall(),
        &|| at(0),
        &WorkerId::new(worker),
        result,
    )
    .await;
    report.expect("a host the core hears")
}

/// Asserts that a request whose key is no action's is refused INVALID_ARGUMENT before the core
/// hears of it: the callers and tickets of the server name an action.
#[tokio::test(start_paused = true)]
async fn a_request_that_keys_no_action_is_refused_invalid() {
    let rig = rig();
    let mut bad = submission(1);
    bad.request.key = WorkKey::new("main", "not a digest");

    let refused = dispatch::submit(&rig.core, at(0), &Default::default(), bad);

    let status = refused.expect_err("refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(rig.core.queued_operations(), 0);
}

/// Asserts that each reason the core does not queue a request reaches the client as the
/// status and words the farm's own driver uses: UNAVAILABLE while the core hands its work
/// over or while the state store does not take the request, and FAILED_PRECONDITION once a
/// newer writer holds the store.
#[tokio::test(start_paused = true)]
async fn a_request_the_core_does_not_queue_is_refused_with_the_farms_words() {
    let fenced = rig();
    fenced.core.fence();
    let down = rig();
    down.store.set_down(true);
    let superseded = rig();
    superseded.store.set_fenced(true);

    let told = |rig| submit(&rig, 1, 0).expect_err("refused");

    let status = told(fenced);
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(
        status.message(),
        "this frontend is handing its work to its successor"
    );
    let status = told(down);
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(
        status.message(),
        "the scheduler's state store did not take the request"
    );
    let status = told(superseded);
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        status.message(),
        "this scheduler was superseded by a newer writer of the state store"
    );
}

/// Asserts that a request for the action of an operation whose finish the state store has
/// not taken yet is refused UNAVAILABLE, in the farm's words, and queues nothing: taking the
/// rows up again would run the action twice. The result that is held is not acknowledged,
/// and neither is its copy.
#[tokio::test(start_paused = true)]
async fn a_request_for_an_action_whose_finish_is_held_is_refused_until_it_is_written() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    submit(&rig, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    rig.store.set_down(true);

    let ack = report_of(&rig, "node-a", ran(lease, 0)).await;
    let copy = report_of(&rig, "node-a", ran(lease, 0)).await;
    rig.store.set_down(false);
    let status = submit(&rig, 1, 0).expect_err("refused");

    assert_eq!((ack, copy), (None, None), "neither is acknowledged");
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(
        status.message(),
        "the action's previous run is finishing; the request is not queued"
    );
}

/// Asserts that a `Result` with no lease id is not acknowledged: there is nothing to name.
#[tokio::test(start_paused = true)]
async fn a_result_without_a_lease_id_gets_no_ack() {
    let rig = rig();
    let mut result = ran(LeaseId::new(7, 0), 0);
    result.lease_id = None;

    assert_eq!(report_of(&rig, "node-a", result).await, None);
}

/// Asserts that a `Result` for a lease no one holds is refused as unknown, one from a node
/// that does not hold the lease is refused as stale, one that names an action other than the
/// lease's is refused as a mismatch (and does not finish the operation), one that names the
/// lease's own action is accepted, and the copy of one the core answered is refused as a
/// duplicate.
#[tokio::test(start_paused = true)]
async fn a_result_is_refused_for_an_unknown_lease_another_action_or_a_copy() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    register(&rig, "node-b", 0);
    submit(&rig, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    let reason = |ack: Option<worker::ResultAck>| {
        let ack = ack.expect("an ack");
        (
            ack.accepted,
            ResultAckReason::try_from(ack.reason).expect("a reason"),
        )
    };

    let unknown = report_of(&rig, "node-a", ran(LeaseId::new(7, 500), 0)).await;
    let stale = report_of(&rig, "node-b", ran(lease, 0)).await;
    let mut other = ran(lease, 0);
    other.action_digest = Some(kbf_front::digest_to_proto(
        &ActionKey::try_from(&request(2).key)
            .expect("an action")
            .action,
    ));
    let mismatch = report_of(&rig, "node-a", other).await;
    let mut right = ran(lease, 0);
    right.action_digest = Some(kbf_front::digest_to_proto(
        &ActionKey::try_from(&request(1).key)
            .expect("an action")
            .action,
    ));
    let first = report_of(&rig, "node-a", right).await;
    let copy = report_of(&rig, "node-a", ran(lease, 0)).await;

    assert_eq!(reason(unknown), (false, ResultAckReason::UnknownLease));
    assert_eq!(reason(stale), (false, ResultAckReason::Stale));
    assert_eq!(reason(mismatch), (false, ResultAckReason::Mismatch));
    assert_eq!(reason(first), (true, ResultAckReason::Accepted));
    assert_eq!(reason(copy), (false, ResultAckReason::Duplicate));
}

/// Asserts that a result a node sends for a lease that was taken up from the state store,
/// which the adapter never sent, is accepted: its action is the first caller's, taken from
/// the request the core passed as it took the operation up.
#[tokio::test(start_paused = true)]
async fn a_lease_taken_up_from_the_store_is_answered_for_the_action_of_its_first_caller() {
    let store = Flaky::new();
    let first = rig_over(&store, kbf_front::Recorder::off());
    let mut node = register(&first, "node-a", 0);
    submit(&first, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    drop(first);
    let second = rig_over(&store, kbf_front::Recorder::off());
    register(&second, "node-a", 0);

    let ack = report_of(&second, "node-a", ran(lease, 0))
        .await
        .expect("an ack");

    assert!(ack.accepted, "{ack:?}");
}

/// Asserts that a result the state store could not take when it came is not acknowledged and
/// does not answer its callers then, and that both happen once the record is written: the
/// callers are answered, and the node that sent the result is sent its acknowledgement.
#[tokio::test(start_paused = true)]
async fn a_held_result_is_acknowledged_and_answered_once_its_record_is_written() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    let mut ticket = submit(&rig, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    rig.store.set_down(true);

    let ack = report_of(&rig, "node-a", ran(lease, 0)).await;

    assert_eq!(ack, None);
    assert_eq!(
        *ticket.stage.borrow_and_update(),
        Stage::Executing,
        "not answered yet"
    );
    assert!(node.sent().is_empty(), "nothing is acknowledged");
    rig.store.set_down(false);
    rig.core.tick(at(0)).await;
    assert!(
        matches!(&*ticket.stage.borrow(), Stage::Done(_)),
        "answered"
    );
    let acknowledged = node.sent();
    let [server_message::Message::ResultAck(ack)] = &acknowledged[..] else {
        panic!("one acknowledgement expected: {acknowledged:?}");
    };
    assert!(ack.accepted);
    assert_eq!(
        ack.lease_id,
        Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq
        })
    );
}

/// Asserts that a core is not opened over a state store that cannot be written: the farm
/// is told, and does not start on a store it cannot keep its rows in.
#[test]
fn a_core_is_not_opened_over_a_store_that_cannot_be_written() {
    let store = Flaky::new();
    store.set_down(true);
    let shared: std::sync::Arc<dyn kbf_state::Store> = store;
    let backend: kbf_front::Backend = std::sync::Arc::new(kbf_front::Cache::memory()).into();
    let adapter = super::KbfAdapter::new(7, backend, shared.clone(), wall());
    let config = super::open::config(
        7,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
        crate::PlacementLoad::Off,
    );

    let opened = super::open::open(&config, adapter, shared, crate::EpochPolicy::Keep, at(0));

    assert!(opened.is_err());
}

/// Asserts that an accepted result whose action-cache write fails still answers its callers
/// and is still acknowledged: the run succeeded, only the cache misses it.
#[tokio::test(start_paused = true)]
async fn a_failed_cache_write_still_answers_the_callers() {
    let cache = failing_cache();
    let rig = rig_caching(
        &Flaky::new(),
        kbf_front::Recorder::off(),
        cache.clone().into(),
    );
    let mut node = register(&rig, "node-a", 0);
    let mut ticket = submit(&rig, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    cache.meta().fail_next_action_write();

    let ack = report_of(&rig, "node-a", ran(lease, 0))
        .await
        .expect("an ack");

    assert!(ack.accepted);
    assert!(matches!(&*ticket.stage.borrow_and_update(), Stage::Done(_)));
    let action = ActionKey::try_from(&request(1).key)
        .expect("an action")
        .action;
    let cached = cache.action_result(&action).await;
    assert!(matches!(cached, Ok(None)), "not cached: {cached:?}");
}

/// Asserts that a call the admission refuses is refused with its reason, for each of the
/// calls the farm makes on the core: a `Hello`, a resent `Hello`, a heartbeat and a `Result`.
#[tokio::test(start_paused = true)]
async fn calls_the_admission_refuses_are_refused() {
    let open = rig();
    let wire = register(&open, "node-a", 0);
    let rig = denying(rig());
    let reported = || {
        let caps =
            kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
        crate::farm::Reported::serving(kbf_types::Resources::new(1_000, 1 << 30), caps)
    };
    let connection = || {
        let (outbound, _inbox) = tokio::sync::mpsc::unbounded_channel();
        let welcome = worker::ServerMessage { message: None };
        super::Connection { outbound, welcome }
    };
    let worker = WorkerId::new("node-a");
    let daemon = || kbf_sched::DaemonInstance::new("daemon");

    let registered = dispatch::register(
        &rig.core,
        at(0),
        &worker,
        daemon(),
        daemon(),
        reported(),
        connection(),
    );
    let resized = dispatch::resize(&rig.core, at(0), &worker, wire.stream, reported());
    let beat = rig
        .core
        .heartbeat(at(0), &worker, wire.stream, Beat::new(1, Vec::new()))
        .await;
    let result = dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        &|| at(0),
        &worker,
        ran(LeaseId::new(7, 0), 0),
    )
    .await;

    assert_eq!(registered.expect_err("denied").reason, "denied by the test");
    assert!(resized.is_err());
    assert!(beat.is_err());
    assert!(result.is_err());
}

/// Asserts that a name that is not `operations/<id>` is answered by no ticket, and that
/// one the core holds is answered through its first caller: the name, the instance and the
/// action of the operation, and its stage as it changes.
#[tokio::test(start_paused = true)]
async fn a_name_is_answered_through_the_first_caller_the_core_holds() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    let ticket = submit(&rig, 1, 0).expect("queued");
    assert!(started(&node.sent()).is_some());

    let held = dispatch::wait(&rig.core, &ticket.name).expect("held");

    assert_eq!(held.name, ticket.name);
    assert_eq!(held.instance, "main");
    assert_eq!(held.action, ticket.action);
    assert_eq!(*held.stage.borrow(), Stage::Executing);
    for refused in ["operations/0-1", "operations/x", "x", ""] {
        assert!(dispatch::wait(&rig.core, refused).is_none(), "{refused:?}");
    }
}

/// Asserts that a name the core does not hold is answered from the state store: a finished
/// operation by its `done/` and `doneof/` rows, an operation another frontend queued by its
/// `queue/` row (shown queued), and no ticket for an operation the store has no trace of,
/// whose rows cannot be read, or whose result cannot be read.
#[tokio::test(start_paused = true)]
async fn a_name_the_core_does_not_hold_is_answered_from_the_store() {
    let store = Flaky::new();
    let first = rig_over(&store, kbf_front::Recorder::off());
    let mut node = register(&first, "node-a", 0);
    let finished = submit(&first, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    report_of(&first, "node-a", ran(lease, 0))
        .await
        .expect("an ack");
    let running = submit(&first, 3, 0).expect("queued");
    assert!(started(&node.sent()).is_some(), "leased on the node");
    let second = rig_over(&store, kbf_front::Recorder::off());
    let other = rig_over(&store, kbf_front::Recorder::off());
    let queued = submit(&other, 2, 0).expect("queued elsewhere");

    let done = dispatch::wait(&second.core, &finished.name).expect("finished");
    let waiting = dispatch::wait(&second.core, &queued.name).expect("queued elsewhere");
    let leased = dispatch::wait(&second.core, &running.name).expect("leased elsewhere");
    let unknown = dispatch::wait(&second.core, "operations/99");
    assert_eq!(*leased.stage.borrow(), Stage::Executing);

    assert!(matches!(&*done.stage.borrow(), Stage::Done(_)));
    assert_eq!(
        done.action,
        ActionKey::try_from(&request(1).key)
            .expect("an action")
            .action
    );
    assert_eq!(*waiting.stage.borrow(), Stage::Queued);
    assert!(unknown.is_none());
    let op = OperationId(0);
    store.fail_gets(Some("doneof/"));
    assert!(
        dispatch::wait(&second.core, &finished.name).is_none(),
        "action unreadable"
    );
    store.fail_gets(None);
    store.fail_reads(Some("done/"));
    assert!(
        dispatch::wait(&second.core, &finished.name).is_none(),
        "progress unreadable"
    );
    store.fail_reads(None);
    let garbage = b"garbage".to_vec();
    super::testing::overwrite(store.plain(), &js_store::keys::done(op), garbage);
    assert!(
        dispatch::wait(&second.core, &finished.name).is_none(),
        "result unreadable: not found, so the client sends a fresh Execute call"
    );
}

/// Asserts that a resent `Hello` changes what the node is given: a node that reports itself
/// quarantined is offered nothing, and is offered work again once it reports serving.
#[tokio::test(start_paused = true)]
async fn a_resent_hello_changes_the_work_a_node_is_given() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    let worker = WorkerId::new("node-a");
    let hello = |phase| {
        let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("caps")
            .with_drivers(["container"]);
        crate::farm::Reported {
            phase,
            ..crate::farm::Reported::serving(kbf_types::Resources::new(4_000, 8 << 30), caps)
        }
    };
    let quarantined = kbf_sched::NodePhase::Quarantined("disk full".to_owned());

    dispatch::resize(&rig.core, at(0), &worker, node.stream, hello(quarantined)).expect("heard");
    let ticket = submit(&rig, 1, 0).expect("queued");
    assert!(
        started(&node.sent()).is_none(),
        "a quarantined node is offered nothing"
    );
    assert_ne!(*ticket.stage.borrow(), Stage::Executing);
    dispatch::resize(
        &rig.core,
        at(0),
        &worker,
        node.stream,
        hello(kbf_sched::NodePhase::Serving),
    )
    .expect("heard");
    rig.core.tick(at(0)).await;

    assert!(started(&node.sent()).is_some(), "offered work again");
    assert_eq!(*ticket.stage.borrow(), Stage::Executing);
}

/// Asserts that a result the core took after it was handed over to a successor, which came
/// between the check of the lease and the feed (the outputs of the result were being
/// checked), is refused as stale: the successor, not this core, answers the operation.
#[tokio::test(start_paused = true)]
async fn a_result_taken_after_the_core_was_handed_over_is_refused_stale() {
    let rig = rig();
    let mut node = register(&rig, "node-a", 0);
    let ticket = submit(&rig, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    let now = || {
        rig.core.fence();
        at(0)
    };

    let report = dispatch::report(
        &rig.core,
        &rig.backend,
        &wall(),
        &now,
        &WorkerId::new("node-a"),
        ran(lease, 0),
    )
    .await;

    let ack = report.expect("heard").expect("an ack");
    assert!(!ack.accepted);
    assert_eq!(ack.reason, ResultAckReason::Stale as i32);
    assert_eq!(*ticket.stage.borrow(), Stage::Executing, "not answered");
}

/// Asserts that only the result of a cacheable action that exited zero is written to the
/// action cache: one that exited non-zero is answered, and so is one the client asked not to
/// cache, and neither is cached.
#[tokio::test(start_paused = true)]
async fn only_a_zero_exit_of_a_cacheable_action_is_cached() {
    let cache = Arc::new(Cache::memory());
    let rig = rig_caching(
        &Flaky::new(),
        kbf_front::Recorder::off(),
        cache.clone().into(),
    );
    let mut node = register(&rig, "node-a", 0);
    let no_cache = Submission {
        request: Request {
            do_not_cache: true,
            ..request(3)
        },
        ..submission(3)
    };
    for tag in [1, 2] {
        submit(&rig, tag, 0).expect("queued");
    }
    dispatch::submit(&rig.core, at(0), &Shapes::default(), no_cache).expect("queued");
    let leases = starts(&node.sent());
    assert_eq!(leases.len(), 3);

    for (lease, code) in leases.into_iter().zip([0, 2, 0]) {
        report_of(&rig, "node-a", ran(lease, code))
            .await
            .expect("an ack");
    }

    let mut cached = Vec::new();
    for tag in [1, 2, 3] {
        let action = ActionKey::try_from(&request(tag).key)
            .expect("an action")
            .action;
        cached.push(matches!(cache.action_result(&action).await, Ok(Some(_))));
    }
    assert_eq!(cached, [true, false, false]);
}

/// Asserts that a start that bumps the store's epoch takes the next one and a start that keeps
/// it takes the one the store holds, which is what a daemon that reconnects relies on to
/// keep, or drop, the runs of its leases.
#[test]
fn a_start_that_bumps_the_epoch_takes_the_next_one() {
    let store = Flaky::new();
    let epoch = |policy| {
        let backend: kbf_front::Backend = Arc::new(Cache::memory()).into();
        rig_started(&store, kbf_front::Recorder::off(), backend, policy)
            .core
            .epoch()
    };

    let first = epoch(crate::EpochPolicy::Keep);
    let kept = epoch(crate::EpochPolicy::Keep);
    let bumped = epoch(crate::EpochPolicy::Bump);

    assert_eq!(kept, first);
    assert_eq!(bumped, first.map(|epoch| epoch + 1));
}

/// Asserts that an operation whose result expired between the look at its action and the look
/// at where it stands (the `doneof/` row is still there, and nothing else is) gets no ticket:
/// its callers would wait on nothing.
#[tokio::test(start_paused = true)]
async fn an_operation_whose_result_expired_gets_no_ticket() {
    let store = Flaky::new();
    let first = rig_over(&store, kbf_front::Recorder::off());
    let mut node = register(&first, "node-a", 0);
    let finished = submit(&first, 1, 0).expect("queued");
    let lease = started(&node.sent()).expect("started");
    report_of(&first, "node-a", ran(lease, 0))
        .await
        .expect("an ack");
    let second = rig_over(&store, kbf_front::Recorder::off());
    assert!(
        dispatch::wait(&second.core, &finished.name).is_some(),
        "finished: a ticket"
    );
    let operation = crate::machine::parse_stored_name(&finished.name).expect("a stored name");
    let expired = Txn::new().delete(js_store::keys::done(operation), Cond::Any);
    store
        .plain()
        .txn(expired)
        .expect("a store")
        .expect("a write");

    assert!(dispatch::wait(&second.core, &finished.name).is_none());
}

/// Asserts that the time a submit waited for the core's lock is reported to the farm's
/// metrics, once for each submit and for nothing else.
#[tokio::test(start_paused = true)]
async fn a_submit_reports_the_time_it_waited_for_the_lock() {
    let rig = rig();
    let waits = || rig.backend.metrics().sched_lock_wait.count();
    register(&rig, "node-a", 0);
    assert_eq!(waits(), 0);

    submit(&rig, 1, 0).expect("queued");
    assert_eq!(waits(), 1);
    rig.core.tick(at(0)).await;
    assert_eq!(waits(), 1);
}
