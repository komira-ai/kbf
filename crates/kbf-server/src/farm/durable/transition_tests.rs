//! The transitions of an operation in the state store when several frontends share it: a
//! frontend that loses a race to another finds its rows changed or gone and writes
//! nothing, and the other's work stands. Each test stands two handles over one store for
//! two frontends, and lands one frontend's write between the other's reads and its write
//! where the race is in that gap.

use kbf_caps::NodeCaps;
use kbf_types::{Answer, Failure, FencePolicy, LeaseKind, Outcome, Refusal, Resources, StartLease};

use super::blocks_tests::{Racing, durable_over, request};
use super::*;

pub(super) fn subject() -> Subject {
    Subject::of(Described::default()).0
}

/// The grant of lease `seq` of operation `operation` to `worker`.
pub(super) fn start(operation: u64, seq: u64, worker: &str) -> StartLease {
    StartLease {
        worker: WorkerId::new(worker),
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        key: request(1).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: FencePolicy::RunOn,
        device: None,
    }
}

pub(super) fn answer(operation: u64, seq: u64) -> Answer {
    Answer {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        waiters: Vec::new(),
        outcome: Outcome::Failed(Failure::Invalid),
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

pub(super) fn requeue(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::Silent,
    }
}

fn refusal(operation: u64) -> Refusal {
    Refusal {
        operation: OperationId(operation),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    }
}

/// An ending whose result carries `message`, so two endings tell apart.
pub(super) fn ending(message: &str) -> Ending {
    Ending {
        finished: Finished::Failed(kbf_proto::google::rpc::Status {
            code: 3,
            message: message.to_owned(),
            details: Vec::new(),
        }),
        retention: Duration::from_secs(1),
    }
}

pub(super) fn rows(store: &Racing, prefix: &str) -> usize {
    store.scan_prefix(prefix).expect("scan").len()
}

/// What another frontend writes to grant lease `seq` of `operation` to `node-b` over
/// whatever the rows hold.
fn another_grant(operation: u64, seq: u64) -> Txn {
    let grant = LeaseGrant {
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        worker: WorkerId::new("node-b"),
        cost: Default::default(),
    };
    let value = encode(FarmInput::Committed(ControlRecord::Lease(grant))).expect("encodes");
    Txn::new()
        .delete(queue_key(OperationId(operation)), Cond::Any)
        .put(lease_key(LeaseId::new(5, seq)), value, Cond::Any)
}

/// Asserts that a grant the store applied before and did not acknowledge is taken as
/// written when it is tried again, and that a grant is not taken as written when the lease
/// row names another worker or another lease.
#[test]
fn a_grant_written_before_is_taken_as_written_unless_the_lease_is_another() {
    let store = Racing::new();
    let durable = durable_over(&store);
    durable
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    let grant = [Effect::Start(start(4, 0, "node-a"))];
    durable.persist(&grant, None).expect("grant");
    let txns = store.commit_stats().txns;

    durable.persist(&grant, None).expect("again");

    assert_eq!(
        store.commit_stats().txns,
        txns,
        "a written grant wrote again"
    );
    let other_worker = [Effect::Start(start(4, 0, "node-b"))];
    let stranger = durable.persist(&other_worker, None);
    assert!(
        matches!(stranger, Err(StateError::Missing(_))),
        "{stranger:?}"
    );
    let other_lease = [Effect::Start(start(4, 1, "node-a"))];
    let second = durable.persist(&other_lease, None);
    assert!(matches!(second, Err(StateError::Missing(_))), "{second:?}");
}

/// Asserts that a lease given up after another frontend finished the operation puts
/// nothing back in the queue and writes no loss count: the operation is not run again.
#[test]
fn a_lease_given_up_after_another_frontend_finished_the_operation_queues_nothing() {
    let store = Racing::new();
    let (first, second) = (durable_over(&store), durable_over(&store));
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    second
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("finished")))
        .expect("finished");
    let txns = store.commit_stats().txns;

    first
        .requeued(&requeue(4, 0), Some(1), None, &FloorChange::default())
        .expect("nothing to give up");

    assert_eq!(store.commit_stats().txns, txns, "a give-up wrote");
    let queued = (rows(&store, "queue/"), rows(&store, "loss/"));
    assert_eq!(queued, (0, 0));
    assert_eq!((rows(&store, "op/"), rows(&store, "done/")), (0, 1));
}

/// Asserts that a lease given up a second time, by a frontend that did not know another had
/// given it up and granted the operation again, queues nothing and leaves the new lease and
/// the loss count of the first give-up as they are; and that the new lease, given up in its
/// turn, queues the operation and counts as the second loss.
#[test]
fn a_lease_given_up_twice_does_not_queue_an_operation_that_runs_under_a_new_lease() {
    let store = Racing::new();
    let (first, second) = (durable_over(&store), durable_over(&store));
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    first
        .requeued(&requeue(4, 0), Some(1), None, &FloorChange::default())
        .expect("given up");
    second
        .persist(&[Effect::Start(start(4, 1, "node-b"))], None)
        .expect("granted again");

    second
        .requeued(&requeue(4, 0), Some(2), None, &FloorChange::default())
        .expect("a second give-up of the first lease");

    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (0, 1));
    let loss = store.get(&loss_key(OperationId(4))).expect("read");
    assert_eq!(loss.expect("a loss row").value, 1_u64.to_be_bytes());

    // The new lease, given up in its turn, is queued again and counts as the second loss.
    second
        .requeued(&requeue(4, 1), Some(2), None, &FloorChange::default())
        .expect("the new lease given up");
    assert_eq!((rows(&store, "queue/"), rows(&store, "lease/")), (1, 0));
    let loss = store.get(&loss_key(OperationId(4))).expect("read");
    assert_eq!(loss.expect("a loss row").value, 2_u64.to_be_bytes());
}

/// Asserts that a give-up that loses the lease row to another frontend's finish between its
/// read and its write is refused whole, and that tried again it finds the lease row gone
/// and writes nothing: the finished operation is not queued.
#[test]
fn a_give_up_that_loses_to_a_finish_after_it_read_the_rows_writes_nothing() {
    let store = Racing::new();
    let first = durable_over(&store);
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    let lease = Txn::new().delete(lease_key(LeaseId::new(5, 0)), Cond::Any);
    let finish = ending("finished").settle(lease, OperationId(4));
    store.land(&queue_key(OperationId(4)), finish);

    let lost = first.requeued(&requeue(4, 0), Some(1), None, &FloorChange::default());

    assert!(matches!(lost, Err(StateError::Conflict(_))), "{lost:?}");
    first
        .requeued(&requeue(4, 0), Some(1), None, &FloorChange::default())
        .expect("tried again, nothing to give up");
    assert_eq!((rows(&store, "queue/"), rows(&store, "loss/")), (0, 0));
    assert_eq!((rows(&store, "op/"), rows(&store, "done/")), (0, 1));
}

/// Asserts that a result for a lease the farm gave up in the same feed finishes the queued
/// operation: its rows go, its loss count with them, and its result is the one row left.
#[test]
fn a_result_for_a_lease_the_farm_gave_up_finishes_the_queued_operation() {
    let store = Racing::new();
    let durable = durable_over(&store);
    durable
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    durable
        .requeued(&requeue(4, 0), Some(1), None, &FloorChange::default())
        .expect("given up");

    durable
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("ran")))
        .expect("finished");

    let gone = (rows(&store, "op/"), rows(&store, "queue/"));
    assert_eq!((gone, rows(&store, "lease/")), ((0, 0), 0));
    assert_eq!((rows(&store, "loss/"), rows(&store, "done/")), (0, 1));
}

/// Asserts that a result for a lease the farm gave up, which loses the queue row to another
/// frontend's grant between its read and its write, is stale: the operation runs under
/// the new lease, and nothing of it is finished or removed.
#[test]
fn a_result_that_loses_to_a_grant_of_another_frontend_after_it_read_the_rows_is_stale() {
    let store = Racing::new();
    let first = durable_over(&store);
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    first
        .requeued(&requeue(4, 0), None, None, &FloorChange::default())
        .expect("given up");
    store.land(&queue_key(OperationId(4)), another_grant(4, 1));

    let late = first.persist(&[Effect::Answer(answer(4, 0))], Some(&ending("late")));

    assert!(matches!(late, Err(StateError::Missing(_))), "{late:?}");
    assert_eq!((rows(&store, "op/"), rows(&store, "lease/")), (1, 1));
    assert_eq!(rows(&store, "done/"), 0);
}

/// Asserts that a finish repeated by another frontend (a daemon's result that reaches two
/// of them) is acknowledged as a duplicate, writes nothing, and leaves the first result
/// standing.
#[test]
fn a_finish_repeated_by_another_frontend_is_acknowledged_and_the_first_result_stands() {
    let store = Racing::new();
    let (first, second) = (durable_over(&store), durable_over(&store));
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    first
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("first")))
        .expect("finished");
    let txns = store.commit_stats().txns;

    second
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("second")))
        .expect("a duplicate is acknowledged");

    assert_eq!(store.commit_stats().txns, txns, "a duplicate wrote");
    assert_eq!(
        second.progress(OperationId(4)).expect("progress"),
        Progress::Ended(ending("first").finished)
    );
}

/// Asserts that a result is accepted when another frontend's give-up of the lease lands
/// between its read of the lease row and its write: the operation, queued again by the
/// other, is finished, and its result is the row left.
#[test]
fn a_result_that_loses_the_lease_row_to_a_give_up_of_another_frontend_still_finishes() {
    let store = Racing::new();
    let first = durable_over(&store);
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    let lease = lease_key(LeaseId::new(5, 0));
    let given_up = Txn::new().delete(lease.clone(), Cond::Any).put(
        queue_key(OperationId(4)),
        Vec::new(),
        Cond::Absent,
    );
    store.land(&lease, given_up);

    first
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("ran")))
        .expect("finished");

    assert_eq!((rows(&store, "op/"), rows(&store, "queue/")), (0, 0));
    assert_eq!((rows(&store, "lease/"), rows(&store, "done/")), (0, 1));
}

/// Asserts that a result is acknowledged as a duplicate when another frontend's finish
/// lands between its read of the lease row and its write: the first result stands.
#[test]
fn a_result_that_loses_the_lease_row_to_a_finish_of_another_frontend_is_a_duplicate() {
    let store = Racing::new();
    let first = durable_over(&store);
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    let lease = lease_key(LeaseId::new(5, 0));
    let gone = Txn::new().delete(lease.clone(), Cond::Any);
    store.land(&lease, ending("other").settle(gone, OperationId(4)));

    first
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("mine")))
        .expect("a duplicate is acknowledged");

    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        Progress::Ended(ending("other").finished)
    );
}

/// Asserts that a finish whose queue and operation rows another frontend's finish removed
/// between its read and its write is acknowledged as a duplicate when a result is there,
/// the other's standing.
#[test]
fn a_finish_that_loses_the_rows_to_another_frontends_finish_is_a_duplicate() {
    let store = Racing::new();
    let first = durable_over(&store);
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    first
        .persist(&[Effect::Start(start(4, 0, "node-a"))], None)
        .expect("grant");
    first
        .requeued(&requeue(4, 0), None, None, &FloorChange::default())
        .expect("given up");
    let finish = ending("other").settle(Txn::new(), OperationId(4));
    store.land(&queue_key(OperationId(4)), finish);

    first
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("mine")))
        .expect("a duplicate is acknowledged");

    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        Progress::Ended(ending("other").finished)
    );
}

/// Asserts that a refusal of an operation another frontend has granted is stale and changes
/// nothing: the operation is not refused while another frontend runs it.
#[test]
fn a_refusal_of_an_operation_another_frontend_runs_is_stale_and_changes_nothing() {
    let store = Racing::new();
    let (first, second) = (durable_over(&store), durable_over(&store));
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    second
        .persist(&[Effect::Start(start(4, 0, "node-b"))], None)
        .expect("granted by the other");
    let txns = store.commit_stats().txns;

    let refused = first.persist(&[Effect::Refuse(refusal(4))], Some(&ending("refused")));

    assert!(
        matches!(refused, Err(StateError::Missing(_))),
        "{refused:?}"
    );
    assert_eq!(store.commit_stats().txns, txns, "a refusal wrote");
    assert_eq!((rows(&store, "op/"), rows(&store, "lease/")), (1, 1));
    assert_eq!(rows(&store, "done/"), 0);
}

/// Asserts that a refusal that loses the queue row to another frontend's grant between its
/// read and its write is stale too: the grant stands, and the operation is not refused.
#[test]
fn a_refusal_that_loses_to_a_grant_after_it_read_the_rows_is_stale() {
    let store = Racing::new();
    let first = durable_over(&store);
    first
        .submitted(OperationId(4), &request(1), &subject())
        .expect("submit");
    store.land(&queue_key(OperationId(4)), another_grant(4, 0));

    let refused = first.persist(&[Effect::Refuse(refusal(4))], Some(&ending("refused")));

    assert!(
        matches!(refused, Err(StateError::Missing(_))),
        "{refused:?}"
    );
    assert_eq!((rows(&store, "op/"), rows(&store, "lease/")), (1, 1));
    assert_eq!(rows(&store, "done/"), 0);
}

/// Asserts that a refusal that finds another result for its operation is stale and that
/// result stands, and that a refusal repeated after it was written (the acknowledgement
/// was lost) is acknowledged and writes nothing.
#[test]
fn a_refusal_is_a_duplicate_only_of_itself() {
    let store = Racing::new();
    let (first, second) = (durable_over(&store), durable_over(&store));
    for operation in [4, 5] {
        first
            .submitted(OperationId(operation), &request(1), &subject())
            .expect("submit");
    }
    second
        .persist(&[Effect::Start(start(4, 0, "node-b"))], None)
        .expect("granted by the other");
    second
        .persist(&[Effect::Answer(answer(4, 0))], Some(&ending("ran")))
        .expect("finished by the other");

    let late = first.persist(&[Effect::Refuse(refusal(4))], Some(&ending("refused")));

    assert!(matches!(late, Err(StateError::Missing(_))), "{late:?}");
    assert_eq!(
        first.progress(OperationId(4)).expect("progress"),
        Progress::Ended(ending("ran").finished)
    );
    let refuse = [Effect::Refuse(refusal(5))];
    first
        .persist(&refuse, Some(&ending("refused")))
        .expect("refused");
    let txns = store.commit_stats().txns;
    first
        .persist(&refuse, Some(&ending("refused")))
        .expect("refused again");
    assert_eq!(store.commit_stats().txns, txns, "a repeated refusal wrote");
}

/// Asserts that an operation could run if any daemon session in the store could run it,
/// whichever frontend wrote the session; and that none could when the store holds no
/// session, or only sessions too small for it or of another platform.
#[test]
fn a_request_could_run_if_any_daemon_session_of_the_store_could_run_it() {
    let store = Racing::new();
    let (mine, other) = (durable_over(&store), durable_over(&store));
    assert!(!mine.session_could_run(&request(1)).expect("no sessions"));
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 << 30), caps);
    let daemon = DaemonInstance::new("daemon");
    other
        .session(&WorkerId::new("node-b"), &daemon, false, &reported)
        .expect("a session of the other frontend");

    assert!(mine.session_could_run(&request(1)).expect("a session"));
    let mut big = request(1);
    big.resources = Resources::new(1_000, 16 << 30);
    assert!(!mine.session_could_run(&big).expect("a session"));
    let mut mac = request(1);
    let needs = kbf_caps::Request::from_platform([("OSFamily", "macos")]);
    mac.needs = needs.expect("a platform");
    assert!(!mine.session_could_run(&mac).expect("a session"));
}

/// Asserts that a session row that is not a daemon's registration is refused naming the
/// row, whether its bytes are not a scheduler input at all or are another input.
#[test]
fn a_session_row_that_is_not_a_registration_is_refused_naming_it() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let submit = encode(FarmInput::Submit(request(1))).expect("encodes");
    for (key, value) in [("session/a", vec![0xff]), ("session/b", submit)] {
        store
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("put");

        let read = durable.session_could_run(&request(1));

        let error = read.expect_err("a corrupt row");
        assert!(error.to_string().contains(key), "{error}");
        store
            .txn(Txn::new().delete(key, Cond::Any))
            .expect("store")
            .expect("delete");
    }
}

/// Asserts that the daemons of the other frontends are the session rows of the nodes this
/// frontend does not hold: a node in `connected` is left out, the others are listed with
/// their capacity, and a row that is not a registration is refused naming it.
#[test]
fn the_remote_sessions_are_the_rows_of_nodes_this_frontend_does_not_hold() {
    let store = Racing::new();
    let (mine, other) = (durable_over(&store), durable_over(&store));
    let none = std::collections::BTreeSet::new();
    assert!(mine.remote_sessions(&none).expect("no sessions").is_empty());
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 << 30), caps);
    let daemon = DaemonInstance::new("daemon");
    for node in ["node-a", "node-b"] {
        other
            .session(&WorkerId::new(node), &daemon, false, &reported)
            .expect("a session");
    }

    let held = [WorkerId::new("node-a")].into();
    let remote = mine.remote_sessions(&held).expect("sessions");
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].1, Resources::new(4_000, 8 << 30));
    assert_eq!(mine.remote_sessions(&none).expect("sessions").len(), 2);

    store
        .txn(Txn::new().put("session/c", vec![0xff], Cond::Any))
        .expect("store")
        .expect("put");
    let error = mine.remote_sessions(&none).expect_err("a corrupt row");
    assert!(error.to_string().contains("session/c"), "{error}");
    // A row that decodes to another scheduler input is refused as well.
    let submit = encode(FarmInput::Submit(request(1))).expect("encodes");
    store
        .txn(Txn::new().put("session/c", submit, Cond::Any))
        .expect("store")
        .expect("put");
    let error = mine.remote_sessions(&none).expect_err("another input");
    assert!(error.to_string().contains("session/c"), "{error}");
}
