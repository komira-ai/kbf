//! Finding an operation by its name: the `doneof/` row that keeps the action of a result, and
//! the look that says what an operation runs and where it stands from the store's rows alone.

use kbf_types::{ActionKey, Refusal};

use super::blocks_tests::{Racing, action_key, durable_over, request};
use super::named::{decode, encode};
use super::owner_tests::{grant, overwrite, remove, value_of};
use super::transition_tests::{answer, ending, rows, subject};
use super::*;

/// Operation 4 of `request(1)`, queued.
fn queued(durable: &Durable) {
    let submitted = durable.submitted(OperationId(4), &request(1), &subject());
    submitted.expect("submit");
}

/// Operation 4 of `request(1)`, finished as `ending("ran")` says.
fn finished(durable: &Durable) {
    queued(durable);
    grant(durable, 4, 0, "node-a").expect("grant");
    let ended = ending("ran");
    let result = [Effect::Answer(answer(4, 0))];
    durable.persist(&result, Some(&ended)).expect("finished");
}

/// What the store says of operation `operation`.
fn named(durable: &Durable, operation: u64) -> Option<(ActionKey, Progress)> {
    durable
        .named_operation(OperationId(operation))
        .expect("a look")
}

/// Asserts that a `doneof/` row is a version byte, the action's size, hash and then the
/// instance name, pinned byte for byte because another release reads it, that it reads back
/// as the action it was made from, and that a row that is none of that is refused naming
/// what is wrong with it.
#[test]
fn a_doneof_row_is_pinned_byte_for_byte_and_refuses_what_it_cannot_read() {
    let key = action_key(1);
    let mut row = vec![1_u8, 0, 0, 0, 0, 0, 0, 0, 1];
    row.extend([1_u8; 32]);
    row.extend(b"main");
    let mut short_hash = vec![1_u8];
    short_hash.extend([0_u8; 8]);
    short_hash.extend([7_u8; 31]);
    let mut bad_name = row.clone();
    bad_name.push(0xff);

    assert_eq!(encode(&key), row);
    assert_eq!(decode(&row), Ok(key));
    let refused = [
        (Vec::new(), "not a version 1 action row"),
        (vec![2_u8], "not a version 1 action row"),
        (vec![1_u8, 0, 0], "an action row is cut short"),
        (short_hash, "an action row is cut short"),
        (bad_name, "an instance name is not UTF-8"),
    ];
    for (row, why) in refused {
        assert_eq!(decode(&row), Err(why), "{row:?}");
    }
}

/// Asserts that the transaction that finishes an operation puts a `doneof/` row beside its
/// `done/` row, naming the action its `op/` row held, and that the two live as long: still
/// there a second before the floor of the result's life, both gone a second after.
#[test]
fn a_finish_puts_the_action_beside_its_result_for_as_long() {
    let store = Racing::new();
    let durable = durable_over(&store);

    finished(&durable);

    let kept = (
        rows(&store, "op/"),
        rows(&store, "done/"),
        rows(&store, "doneof/"),
    );
    assert_eq!(kept, (0, 1, 1));
    let row = value_of(&store, "doneof/0000000000000004");
    assert_eq!(row, encode(&action_key(1)));
    store.advance(DONE_FLOOR - Duration::from_secs(1));
    assert_eq!((rows(&store, "done/"), rows(&store, "doneof/")), (1, 1));
    store.advance(Duration::from_secs(2));
    assert_eq!((rows(&store, "done/"), rows(&store, "doneof/")), (0, 0));
}

/// Asserts that a refusal puts the `doneof/` row too: an operation refused for want of a
/// daemon is found by its name afterwards, with the reason it was refused for.
#[test]
fn a_refusal_puts_the_action_beside_its_result_too() {
    let store = Racing::new();
    let durable = durable_over(&store);
    queued(&durable);
    let refusal = Refusal {
        operation: OperationId(4),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    };

    durable
        .persist(&[Effect::Refuse(refusal)], Some(&ending("refused")))
        .expect("refused");

    let found = named(&durable, 4).expect("found by name");
    assert_eq!(found.0, action_key(1));
    assert_eq!(found.1, Progress::Ended(ending("refused").finished));
}

/// Asserts that a finish of an operation whose `op/` row is gone, which cannot say what the
/// operation ran, puts no `doneof/` row, and that the operation is then not found by name
/// (the store cannot say which instance to authorize a client against).
#[test]
fn a_result_with_no_action_row_is_not_found_by_name() {
    let store = Racing::new();
    let durable = durable_over(&store);
    queued(&durable);
    grant(&durable, 4, 0, "node-a").expect("grant");
    remove(&store, "op/0000000000000004");

    let result = [Effect::Answer(answer(4, 0))];
    durable
        .persist(&result, Some(&ending("ran")))
        .expect("finished");

    assert_eq!((rows(&store, "done/"), rows(&store, "doneof/")), (1, 0));
    assert_eq!(named(&durable, 4), None);
}

/// Asserts that the look says what the operation runs and where it stands through its
/// life: queued, leased, ended with its result (the action then from the `doneof/` row),
/// and not at all once the result has expired or for an operation nobody queued.
#[test]
fn the_look_follows_an_operation_from_the_queue_to_its_expired_result() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let key = action_key(1);
    queued(&durable);
    assert_eq!(named(&durable, 4), Some((key.clone(), Progress::Queued)));
    grant(&durable, 4, 0, "node-a").expect("grant");
    assert_eq!(named(&durable, 4), Some((key.clone(), Progress::Running)));
    let ended = ending("ran");
    let result = [Effect::Answer(answer(4, 0))];
    durable.persist(&result, Some(&ended)).expect("finished");
    let progress = Progress::Ended(ended.finished);
    assert_eq!(named(&durable, 4), Some((key, progress)));

    assert_eq!(named(&durable, 9), None, "an operation nobody queued");
    store.advance(DONE_FLOOR + Duration::from_secs(1));
    assert_eq!(named(&durable, 4), None, "a result past its life");
}

/// Asserts that noting the action of an operation fails, and adds nothing to the transaction,
/// when the `op/` row cannot be read and when it is not a request: a finish never puts a
/// `doneof/` row on a guess.
#[test]
fn noting_the_action_fails_on_an_op_row_it_cannot_read() {
    let store = Racing::new();
    let durable = durable_over(&store);
    queued(&durable);
    let ended = ending("ran");
    store.fail_reads(Some("op/"));

    let unread = durable.noting(Txn::new(), OperationId(4), &ended);

    assert!(unread.is_err(), "an unreadable row was noted");
    store.fail_reads(None);
    overwrite(&store, "op/0000000000000004", vec![0xff]);
    let corrupt = durable.noting(Txn::new(), OperationId(4), &ended);
    let said = corrupt.expect_err("a bad request row").to_string();
    assert!(said.contains("op/0000000000000004"), "{said}");
}

/// Asserts that a look fails, and does not answer on a guess, when the store cannot read a
/// row it needs: the result row of a queued operation, and the row that names the action of
/// a finished one.
#[test]
fn a_look_fails_when_the_store_cannot_read_what_it_needs() {
    let store = Racing::new();
    let durable = durable_over(&store);
    queued(&durable);
    store.fail_reads(Some("done/"));

    let unread = durable.named_operation(OperationId(4));

    assert!(unread.is_err(), "answered without the result row");
    let ended_store = Racing::new();
    let ended = durable_over(&ended_store);
    finished(&ended);
    ended_store.fail_reads(Some("doneof/"));
    let unread = ended.named_operation(OperationId(4));
    assert!(unread.is_err(), "answered without the action row");
}

/// Asserts that a row the scheduler did not write fails the look naming the row, whether it
/// is the `doneof/` row of a finished operation or the `op/` row of an unfinished one: a
/// client is never answered on a guess.
#[test]
fn a_row_that_cannot_be_read_fails_the_look_naming_it() {
    let store = Racing::new();
    let durable = durable_over(&store);
    finished(&durable);
    overwrite(&store, "doneof/0000000000000004", vec![9]);

    let refused = durable.named_operation(OperationId(4));

    let said = refused.expect_err("a bad action row").to_string();
    assert!(said.contains("doneof/0000000000000004"), "{said}");
    assert!(said.contains("not a version 1 action row"), "{said}");
    queued(&durable);
    overwrite(&store, "op/0000000000000004", vec![0xff]);
    let refused = durable.named_operation(OperationId(4));
    let said = refused.expect_err("a bad request row").to_string();
    assert!(said.contains("op/0000000000000004"), "{said}");
}
