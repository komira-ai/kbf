//! The action rows (`exec/act/`): one open operation per action across the frontends of a
//! store, the follow of an operation another frontend runs, and the rows that an earlier
//! release, a lost answer or a race leaves behind.

use kbf_state::ManualClock;
use kbf_types::DigestFunction;

use super::*;
use crate::executor::tests::Faulty;

fn store() -> Arc<Faulty> {
    Arc::new(Faulty::new(Arc::new(ManualClock::default())))
}

fn rows(store: &Arc<Faulty>, instance: &str) -> Rows {
    Rows::new(Arc::clone(store) as Arc<dyn Store>, instance)
}

fn put(store: &Faulty, key: &str, value: &[u8]) {
    store
        .txn(Txn::new().put(key, value.to_vec(), Cond::Any))
        .expect("a write")
        .expect("no conflict");
}

fn raw(store: &Faulty, key: &str) -> Option<Vec<u8>> {
    store.get(key).expect("a read").map(|found| found.value)
}

fn action(instance: &str, tag: u8) -> ActionKey {
    ActionKey {
        instance: instance.to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], 100 + u64::from(tag)),
    }
}

/// The first attempt's row of `action`.
fn first(action: &ActionKey) -> OpRow {
    OpRow {
        owner: String::new(),
        instance: action.instance.clone(),
        hash: action.action.hash_hex(),
        size: action.action.size_bytes,
        attempt: 0,
        job: "job".to_owned(),
        key: Some("cas/b/1/aa".to_owned()),
        whole_machine: false,
    }
}

fn act_of(action: &ActionKey) -> String {
    act_key(
        &action.instance,
        &action.action.hash_hex(),
        action.action.size_bytes,
    )
}

fn names(operation: u64) -> Vec<u8> {
    let mut value = vec![1];
    value.extend(operation.to_be_bytes());
    value
}

fn ran() -> Finished {
    Finished::Ran(Box::default())
}

fn op(n: u64) -> OperationId {
    OperationId(n)
}

/// Catches a second frontend submitting a second job of an action the first one runs (a
/// rebuild): the first open writes the operation's row and the action's row, naming it, in
/// one write; an open of the same action by another frontend joins that operation and writes
/// nothing of its own; the same action under another instance, and another action, are not
/// joined.
#[test]
fn a_second_open_of_an_action_on_another_frontend_joins_the_first() {
    let store = store();
    let (a, b) = (rows(&store, "a"), rows(&store, "b"));
    let main = action("main", 1);
    let batches = store.batches.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(a.open_op(op(1), &first(&main)), Ok(Opened::Ours));
    assert_eq!(
        store.batches.load(std::sync::atomic::Ordering::SeqCst),
        batches + 1,
        "one write"
    );
    assert_eq!(raw(&store, &act_of(&main)), Some(names(1)));
    assert_eq!(a.op(op(1)).expect("a read").expect("a row").owner, "a");

    assert_eq!(b.open_op(op(2), &first(&main)), Ok(Opened::Joined(op(1))));
    assert_eq!(b.op(op(2)), Ok(None), "the joiner wrote its own row");
    assert_eq!(raw(&store, &act_of(&main)), Some(names(1)));

    let other_instance = action("other/instance", 1);
    assert_eq!(b.open_op(op(3), &first(&other_instance)), Ok(Opened::Ours));
    assert_eq!(
        b.open_op(op(4), &first(&action("main", 2))),
        Ok(Opened::Ours)
    );
    assert_eq!(
        act_of(&other_instance),
        format!(
            "exec/act/{}-101/6f746865722f696e7374616e6365",
            "01".repeat(32)
        ),
        "the instance is spelled in hex"
    );
}

/// Catches an action row left after its operation finished, which would send the next Execute
/// of the action to an answer that is not its own: the finish deletes the row in the write
/// that puts the answer, and a later open of the action is the operation's own again. A row
/// naming another operation is left alone by a finish.
#[test]
fn a_finish_deletes_the_action_row_naming_it_and_leaves_one_naming_another() {
    let store = store();
    let a = rows(&store, "a");
    let main = action("main", 1);
    a.open_op(op(1), &first(&main)).expect("opened");
    assert_eq!(a.finish(op(1), &main, &ran(), DONE_FLOOR), Ok(ran()));
    assert_eq!(raw(&store, &act_of(&main)), None);
    assert_eq!(a.done(op(1)).expect("a read"), Some((main.clone(), ran())));
    assert_eq!(a.open_op(op(2), &first(&main)), Ok(Opened::Ours));

    put(&store, &act_of(&main), &names(7));
    a.finish(op(2), &main, &ran(), DONE_FLOOR)
        .expect("finished");
    assert_eq!(raw(&store, &act_of(&main)), Some(names(7)));
}

/// Catches a row that an earlier release left (it finishes an operation without deleting the
/// action row) making every later Execute of the action follow an operation that is done or
/// gone: the stale row is deleted and the open is the operation's own.
#[test]
fn a_row_naming_an_operation_that_is_done_or_gone_is_stale_and_replaced() {
    let store = store();
    let a = rows(&store, "a");
    let main = action("main", 1);
    rows(&store, "earlier")
        .finish(op(5), &main, &ran(), DONE_FLOOR)
        .expect("an answer");
    put(&store, &act_of(&main), &names(5));
    assert_eq!(a.open_op(op(6), &first(&main)), Ok(Opened::Ours));
    assert_eq!(raw(&store, &act_of(&main)), Some(names(6)));

    let gone = action("main", 2);
    put(&store, &act_of(&gone), &names(99));
    assert_eq!(a.open_op(op(7), &first(&gone)), Ok(Opened::Ours));
    assert_eq!(raw(&store, &act_of(&gone)), Some(names(7)));
}

/// Catches an open that gives up the operation, or overwrites a row it cannot read, when the
/// action row is of a version this build does not know: the operation runs on its own, its
/// row written alone, and the row is kept for the release that wrote it.
#[test]
fn an_action_row_this_build_cannot_read_leaves_the_operation_to_run_alone() {
    let store = store();
    let a = rows(&store, "a");
    let main = action("main", 1);
    put(&store, &act_of(&main), &[2, 0, 1]);
    assert_eq!(a.open_op(op(1), &first(&main)), Ok(Opened::Ours));
    assert_eq!(a.op(op(1)).expect("a read").expect("a row").owner, "a");
    assert_eq!(raw(&store, &act_of(&main)), Some(vec![2, 0, 1]));
    put(&store, &act_of(&main), &names(1)[..5]);
    assert_eq!(
        a.open_op(op(2), &first(&main)),
        Ok(Opened::Ours),
        "cut short"
    );
}

/// Catches an open whose answer was lost (the write landed) refusing its own operation when it
/// is sent again: the operation's row is there and ours, so the open is ours. An action row
/// naming the operation itself, with its row gone, is ours too. Another process's row of the
/// operation is still refused.
#[test]
fn an_open_sent_again_is_ours_and_another_process_row_is_refused() {
    let store = store();
    let a = rows(&store, "a");
    let main = action("main", 1);
    assert_eq!(a.open_op(op(1), &first(&main)), Ok(Opened::Ours));
    assert_eq!(a.open_op(op(1), &first(&main)), Ok(Opened::Ours));

    let other = action("main", 2);
    put(&store, &act_of(&other), &names(3));
    assert_eq!(a.open_op(op(3), &first(&other)), Ok(Opened::Ours));
    assert_eq!(a.op(op(3)).expect("a read").expect("a row").owner, "a");

    let third = action("main", 3);
    rows(&store, "b")
        .put_op(op(4), &first(&third))
        .expect("theirs");
    assert_eq!(
        a.open_op(op(4), &first(&third)),
        Err(RowsError::Conflict { key: op_key(op(4)) })
    );
}

/// Catches an open that loops, or fails the Execute, when the action row changes under every
/// try (another writer each time): after its tries the operation runs on its own, its row
/// written alone. A store that fails fails the open.
#[test]
fn an_action_row_that_changes_under_every_try_leaves_the_operation_to_run_alone() {
    let store = store();
    let a = rows(&store, "a");
    let main = action("main", 1);
    rows(&store, "earlier")
        .finish(op(5), &main, &ran(), DONE_FLOOR)
        .expect("an answer");
    put(&store, &act_of(&main), &names(5));
    *store.race.lock().expect("lock") = Some(act_of(&main));
    assert_eq!(a.open_op(op(6), &first(&main)), Ok(Opened::Ours));
    *store.race.lock().expect("lock") = None;
    assert_eq!(a.op(op(6)).expect("a read").expect("a row").owner, "a");
    assert_eq!(raw(&store, &act_of(&main)), Some(names(5)));

    store
        .writes_fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        a.open_op(op(7), &first(&action("main", 2))),
        Err(RowsError::Store(_))
    ));
    store
        .writes_fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let busy = action("main", 3);
    put(&store, &act_of(&busy), &names(8));
    store.fail_nth_read(1);
    assert!(
        matches!(a.open_op(op(9), &first(&busy)), Err(RowsError::Store(_))),
        "the read of the action row"
    );
    store.fail_nth_read(2);
    assert!(
        matches!(a.open_op(op(9), &first(&busy)), Err(RowsError::Store(_))),
        "the look at the operation it names"
    );
    store.fail_nth_write(2);
    assert!(
        matches!(a.open_op(op(9), &first(&busy)), Err(RowsError::Store(_))),
        "the delete of the stale row"
    );
}

/// Catches a finish that gives the operation away, or fails, when the action row changes
/// between its read and the write: the finish tries again, and after its tries puts the answer
/// and leaves the row for the next open to find stale.
#[test]
fn a_finish_whose_action_row_changes_under_it_still_answers() {
    let store = store();
    let a = rows(&store, "a");
    let main = action("main", 1);
    a.open_op(op(1), &first(&main)).expect("opened");
    *store.race.lock().expect("lock") = Some(act_of(&main));
    assert_eq!(a.finish(op(1), &main, &ran(), DONE_FLOOR), Ok(ran()));
    *store.race.lock().expect("lock") = None;
    assert_eq!(a.done(op(1)).expect("a read"), Some((main.clone(), ran())));
    assert_eq!(a.op(op(1)), Ok(None));
    assert_eq!(raw(&store, &act_of(&main)), Some(names(1)));
    assert_eq!(
        rows(&store, "b").open_op(op(2), &first(&main)),
        Ok(Opened::Ours)
    );
}

/// Catches an open that gives up, or follows nothing, when the action row it lost to is gone by
/// the time it reads it (that operation finished in between): it tries again, and the operation
/// is its own.
#[test]
fn an_action_row_gone_before_it_is_read_is_tried_again() {
    let store = store();
    let main = action("main", 1);
    rows(&store, "b")
        .open_op(op(1), &first(&main))
        .expect("theirs");
    let (finisher, finished) = (rows(&store, "b"), main.clone());
    *store.before_read.lock().expect("lock") = Some((
        ACT_PREFIX.to_owned(),
        Box::new(move || {
            finisher
                .finish(op(1), &finished, &ran(), DONE_FLOOR)
                .expect("finished in between");
        }),
    ));
    assert_eq!(
        rows(&store, "a").open_op(op(2), &first(&main)),
        Ok(Opened::Ours)
    );
    assert_eq!(raw(&store, &act_of(&main)), Some(names(2)));
}
