//! Results across frontends: the names a farm gives its operations and what answers them, and
//! a request that joins an operation another frontend leased. The other frontend is a farm
//! over the same store, or a handle that writes the rows as it does.

use kbf_front::{Cache, Dispatch, Submission};
use tonic::Code;

use super::blocks_tests::{Racing, action_key, durable_over, farm_over, request, unfinished};
use super::intake_tests::{caller, done_with, invalid, register};
use super::owner_tests::grant;
use super::transition_tests::{answer, ending, rows, subject};
use super::*;
use crate::machine::{parse_stored_name, stored_name};

/// The lease `farm` holds, which must be the only one.
fn lease_of(farm: &Farm) -> LeaseId {
    let views = farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    view.lease
}

/// What `ticket` shows now.
fn shown(ticket: &kbf_front::Ticket) -> Stage {
    ticket.stage.borrow().clone()
}

/// Asserts that a farm over a state store names an operation by its id, that every caller of
/// the operation (a twin that joined it too) gets that name, and that its own scheduler
/// answers the name with the instance and action the operation runs and its stage as it
/// changes, to the end of the finished retention.
#[tokio::test(start_paused = true)]
async fn a_farm_over_a_store_names_an_operation_by_its_id_and_answers_the_name() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let node = WorkerId::new("node-a");
    let _inbox = register(&farm, "node-a");
    let first = caller(&farm, 1);
    let twin = caller(&farm, 1);
    let [operation] = unfinished(&farm)[..] else {
        panic!("one operation expected");
    };
    let name = stored_name(operation);
    assert_eq!(name, format!("operations/{}", operation.0));
    assert_eq!((&first.name, &twin.name), (&name, &name));

    let waited = farm.wait(&name).expect("held by the scheduler");

    assert_eq!(waited.name, name);
    assert_eq!(waited.instance, "main");
    assert_eq!(waited.action, action_key(1).action);
    assert_eq!(shown(&waited), Stage::Executing);
    assert!(farm.lock().side.watched.is_empty(), "the store was read");
    let lease = lease_of(&farm);
    farm.report(&node, invalid(lease)).await;
    assert_eq!(done_with(&waited), "test");
    let after = farm.wait(&name).expect("kept for the finished retention");
    assert_eq!(done_with(&after), "test");
}

/// Asserts that a name of the old form, `operations/{term}-{waiter}`, is still answered for a
/// caller the process holds, with that caller's instance and action, and for no caller it
/// does not hold.
#[tokio::test(start_paused = true)]
async fn a_name_in_the_old_form_is_answered_for_a_caller_the_process_holds() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let ticket = caller(&farm, 1);
    let old = {
        let state = farm.lock();
        let id = state.side.machine.waiter_ids().next().expect("a caller");
        state.side.machine.name(id)
    };
    assert_ne!(old, ticket.name, "the farm gave a name of the old form");

    let waited = farm.wait(&old).expect("the old name's caller");

    assert_eq!(waited.name, old);
    assert_eq!(waited.instance, "main");
    assert_eq!(waited.action, action_key(1).action);
    let unknown = old.replace("-0", "-7");
    assert!(farm.wait(&unknown).is_none(), "no such caller");
}

/// Asserts that a farm with no state store gives names with its process's term (its ids
/// restart at 0 in every process, so an id alone would name two operations: issue #154), and
/// answers no name of the form `operations/<id>`.
#[tokio::test(start_paused = true)]
async fn a_farm_with_no_state_store_answers_no_name_by_id() {
    let farm = Farm::new_legacy(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let ticket = caller(&farm, 1);
    let [operation] = unfinished(&farm)[..] else {
        panic!("one operation expected");
    };

    assert_eq!(parse_stored_name(&ticket.name), None, "{}", ticket.name);
    assert!(farm.wait(&ticket.name).is_some());
    assert!(farm.wait(&stored_name(operation)).is_none());
}

/// Asserts that a frontend whose scheduler does not hold an operation answers its name from
/// the store: executing while another frontend's daemon runs it, the result once that
/// frontend has recorded it, with the instance and action it ran; that it answers at once,
/// from the result, a client that asks after the finish; and that a name nobody gave is not
/// found.
#[tokio::test(start_paused = true)]
async fn a_frontend_that_does_not_hold_an_operation_answers_its_name_from_the_store() {
    let store = Racing::new();
    let (a, b) = (farm_over(&store), farm_over(&store));
    let node = WorkerId::new("node-a");
    let _inbox = register(&a, "node-a");
    let submitted = caller(&a, 1);
    let lease = lease_of(&a);
    let name = submitted.name.clone();

    let waiting = b.wait(&name).expect("the store holds the operation");

    assert_eq!(waiting.name, name);
    assert_eq!(waiting.instance, "main");
    assert_eq!(waiting.action, action_key(1).action);
    assert_eq!(shown(&waiting), Stage::Executing);
    a.report(&node, invalid(lease)).await;
    b.intake();
    assert_eq!(done_with(&waiting), "test");
    let again = b.wait(&name).expect("the result is kept");
    assert_eq!(done_with(&again), "test");
    assert!(b.wait("operations/987654").is_none());
}

/// Asserts that a request that joins an operation another frontend leased on its node
/// follows it through the store and never takes the lease up: the joiner's scheduler holds
/// nothing, the lease row stands however long the joiner's farm runs on, the joiner is shown
/// the operation executing at once, and gets the result once, from the store, when the other
/// frontend has recorded it.
#[tokio::test(start_paused = true)]
async fn a_request_joining_an_operation_another_frontend_leased_never_takes_its_lease() {
    let store = Racing::new();
    let (a, b) = (farm_over(&store), farm_over(&store));
    let node = WorkerId::new("node-a");
    let _inbox = register(&a, "node-a");
    let leased = caller(&a, 1);
    let lease = lease_of(&a);

    let joined = caller(&b, 1);

    assert_eq!(joined.name, leased.name);
    assert_eq!(shown(&joined), Stage::Executing);
    assert!(unfinished(&b).is_empty(), "the joiner holds the operation");
    assert_eq!(b.lock().intake.following(), 1);
    for _ in 0..3 {
        tokio::time::advance(kbf_sched::fence::LEASE_GRACE).await;
        b.tick().await;
        b.intake();
    }
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));
    assert_eq!(b.dropped_writes(), 0);
    a.report(&node, invalid(lease)).await;
    assert_eq!(shown(&joined), Stage::Executing, "answered before the poll");
    b.intake();
    assert_eq!(done_with(&joined), "test");
    assert_eq!(b.lock().intake.following(), 0);
    assert_eq!(rows(&store, "done/"), 1);
}

/// The outcome the record `key` of `recorder` shows for `action`.
fn outcome_of(
    recorder: &kbf_front::Recorder,
    key: &kbf_invocation::RecordKey,
    action: &kbf_types::Digest,
) -> Option<kbf_invocation::Outcome> {
    let record = recorder.get(key).expect("a record");
    record.row(action).expect("a row").outcome.clone()
}

/// Asserts that a recorded call that joins an operation another frontend leased is ended in
/// the invocation record when the store shows the result: its row has no outcome while the
/// operation runs and the failure its callers were answered with once it has ended, and the
/// farm keeps no note of the operation after.
#[tokio::test(start_paused = true)]
async fn a_recorded_call_that_joined_an_operation_elsewhere_ends_with_the_operation() {
    let store = Racing::new();
    let (a, b) = (farm_over(&store), farm_over(&store));
    let (recorder, recording) = kbf_front::Recorder::new(1 << 20, 64);
    tokio::spawn(recording.run());
    b.lock().side.recording.tell(recorder.clone());
    let node = WorkerId::new("node-a");
    let _inbox = register(&a, "node-a");
    let _leased = caller(&a, 1);
    let lease = lease_of(&a);
    let asked = request(1);
    let action = action_key(1).action;
    let metadata = kbf_front::CallMetadata {
        invocation_id: kbf_front::InvocationId::parse("build-1"),
        ..Default::default()
    };
    let call = kbf_front::ExecuteCall::new(
        &recorder,
        &metadata,
        &Default::default(),
        "main",
        action,
        action,
    )
    .expect("a recorded call");
    let submission = Submission {
        request: asked,
        call: Some(call),
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    };
    let joined = b.submit(submission).expect("joined");
    let key = kbf_invocation::RecordKey::owner(kbf_invocation::InvocationId::new("build-1"));
    recorder.caught_up().await;
    assert_eq!(shown(&joined), Stage::Executing);
    assert_eq!(outcome_of(&recorder, &key, &action), None);

    a.report(&node, invalid(lease)).await;
    b.intake();
    recorder.caught_up().await;

    let failed = kbf_invocation::Outcome::Failed {
        code: Code::InvalidArgument as i32,
        message: "test".to_owned(),
    };
    let ended = outcome_of(&recorder, &key, &action);
    assert_eq!(ended, Some(failed));
    assert!(b.lock().side.recording.is_empty(), "the operation is noted");
}

/// Asserts that a request that joins an operation leased on a node this frontend owns takes
/// the operation up with its lease, as one that joins a queued operation does: the
/// scheduler holds it as an operation of its own, started, and nothing is followed through
/// the store.
#[tokio::test(start_paused = true)]
async fn a_request_joining_an_operation_leased_on_a_node_the_frontend_owns_takes_it_up() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let _inbox = register(&farm, "node-a");
    let other = durable_over(&store);
    let queued = other.submit(OperationId(5000), &request(1), &subject());
    queued.expect("queued by another writer");
    grant(&other, 5000, 0, "node-a").expect("leased on the node");

    let ticket = caller(&farm, 1);

    assert_eq!(ticket.name, "operations/5000");
    assert_eq!(unfinished(&farm), [OperationId(5000)]);
    let state = farm.lock();
    assert!(state.side.machine.sent(LeaseId::new(5, 0)).is_some());
    assert!(!state.sched.queued().any(|op| op == OperationId(5000)));
    assert_eq!(state.intake.following(), 0);
}

/// Asserts that a fenced farm, which takes no intake poll, still follows the operations
/// clients asked it for by name: its follower shows them the result the successor records.
#[tokio::test(start_paused = true)]
async fn a_fenced_farm_follows_the_operations_asked_for_by_name_from_its_follower() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    let queued = other.submit(OperationId(5000), &request(1), &subject());
    queued.expect("queued by the successor");
    grant(&other, 5000, 0, "node-b").expect("leased by the successor");
    farm.fence();
    let waiting = farm.wait("operations/5000").expect("leased in the store");
    assert_eq!(shown(&waiting), Stage::Executing);
    let finished = ending("finished by the successor");
    let result = [Effect::Answer(answer(5000, 0))];
    other
        .persist(&result, Some(&finished))
        .expect("finished by the successor");

    farm.intake();
    assert_eq!(shown(&waiting), Stage::Executing, "a fenced farm polled");
    farm.follow().expect("a look");

    assert_eq!(done_with(&waiting), "finished by the successor");
}

/// Asserts that a name asked for on a frontend that does not hold the operation is followed
/// through the store: the ticket is shown the lease and then the result, a look the store
/// fails leaves it as it was and is made again at the next poll, a name asked for again
/// shares the channel and sees the newest stage at once, the channel is not looked at
/// again once it shows the result, and it is dropped when no ticket holds it. A store that
/// cannot be read answers no name.
#[tokio::test(start_paused = true)]
async fn a_watched_operation_is_followed_through_the_store_until_nobody_holds_a_ticket() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    let queued = other.submit(OperationId(5000), &request(1), &subject());
    queued.expect("queued by another frontend");
    let name = "operations/5000";
    store.fail_reads(Some("op/"));
    assert!(farm.wait(name).is_none(), "a store that fails answered");
    store.fail_reads(None);

    let first = farm.wait(name).expect("queued in the store");

    assert_eq!(shown(&first), Stage::Queued);
    grant(&other, 5000, 0, "node-b").expect("leased by another frontend");
    store.fail_reads(Some("done/"));
    farm.intake();
    assert_eq!(shown(&first), Stage::Queued, "a failed look");
    store.fail_reads(None);
    farm.intake();
    assert_eq!(shown(&first), Stage::Executing);
    let second = farm.wait(name).expect("leased in the store");
    assert_eq!(shown(&second), Stage::Executing);
    assert_eq!(farm.lock().side.watched.len(), 1, "one channel");
    let finished = ending("finished elsewhere");
    let result = [Effect::Answer(answer(5000, 0))];
    other
        .persist(&result, Some(&finished))
        .expect("finished by another frontend");
    farm.intake();
    assert_eq!(done_with(&first), "finished elsewhere");
    assert_eq!(done_with(&second), "finished elsewhere");
    farm.intake();
    assert_eq!(farm.lock().side.watched.len(), 1, "a held channel dropped");
    drop((first, second));
    farm.intake();
    assert!(
        farm.lock().side.watched.is_empty(),
        "an unheld channel kept"
    );
}
