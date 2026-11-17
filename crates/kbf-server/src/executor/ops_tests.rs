//! The table of operations a process holds, on its own: no executor, no job service.

use std::time::Duration;

use kbf_front::Finished;
use kbf_proto::google::rpc;
use kbf_types::{Digest, DigestFunction};

use super::*;

fn key(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    }
}

fn failed(message: &str) -> Finished {
    Finished::Failed(rpc::Status {
        code: 14,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// The ids of a process with no state store count up from its base, and wrap rather than stop.
#[test]
fn ids_count_up_from_the_base_and_wrap_at_the_end_of_the_range() {
    let mut ops = Ops::new(Some(41));
    assert_eq!(ops.next_id(), OperationId(42 - 1));
    assert_eq!(ops.next_id(), OperationId(42));
    let mut last = Ops::new(Some(u64::MAX));
    assert_eq!(last.next_id(), OperationId(u64::MAX));
    assert_eq!(last.next_id(), OperationId(0));
    let mut none = Ops::new(None);
    assert_eq!(none.next_id(), OperationId(0));
}

/// An open operation is joined by key, counted, and known open; one of another key is not.
#[test]
fn an_open_operation_is_joined_by_its_action_and_by_nothing_else() {
    let mut ops = Ops::new(None);
    assert!(ops.join(&key(1)).is_none());
    let _held = ops.open(OperationId(7), key(1));
    assert_eq!(ops.open_count(), 1);
    assert!(ops.is_open(OperationId(7)));
    assert!(!ops.is_open(OperationId(8)));
    let joined = ops.join(&key(1)).expect("joined");
    assert_eq!(joined.operation, OperationId(7));
    assert_eq!(*joined.stage.borrow(), Stage::Queued);
    assert!(ops.join(&key(2)).is_none());
}

/// A key the table maps to an operation it does not hold open is joined by no one: a request
/// then opens an operation of its own.
#[test]
fn a_key_whose_operation_is_not_open_is_not_joined() {
    let mut ops = Ops::new(None);
    ops.by_key.insert(key(1), OperationId(9));
    assert!(ops.join(&key(1)).is_none());
}

/// The callers' stage changes only when what they see changes, and an operation that is not
/// open shows nothing.
#[test]
fn show_changes_the_stage_of_an_open_operation_once() {
    let mut ops = Ops::new(None);
    let seen = ops.open(OperationId(1), key(1)).0;
    let mut watcher = seen.clone();
    watcher.mark_unchanged();
    ops.show(OperationId(1), Stage::Executing);
    assert!(watcher.has_changed().expect("open"));
    assert_eq!(*seen.borrow(), Stage::Executing);
    watcher.mark_unchanged();
    ops.show(OperationId(1), Stage::Executing);
    assert!(
        !watcher.has_changed().expect("open"),
        "the same stage is no change"
    );
    ops.show(OperationId(2), Stage::Executing);
    assert_eq!(ops.open_count(), 1);
}

/// A cancel reaches the operation's task, and says whether the operation is open here.
#[test]
fn a_cancel_is_seen_by_the_task_of_an_open_operation() {
    let mut ops = Ops::new(None);
    let (_, cancelled) = ops.open(OperationId(1), key(1));
    assert!(!*cancelled.borrow());
    assert!(ops.cancel(OperationId(1)));
    assert!(*cancelled.borrow());
    assert!(!ops.cancel(OperationId(2)));
}

/// A finished operation answers its callers, is no longer open or joined, and is kept for a
/// name; finishing one that is not open does nothing.
#[tokio::test(start_paused = true)]
async fn a_finished_operation_is_answered_closed_and_kept_by_name() {
    let mut ops = Ops::new(None);
    let (seen, _) = ops.open(OperationId(1), key(1));
    ops.finish(OperationId(1), failed("a"), Instant::now());
    assert_eq!(*seen.borrow(), Stage::Done(failed("a")));
    assert_eq!(ops.open_count(), 0);
    assert!(ops.join(&key(1)).is_none());
    let ticket = ops.ticket(OperationId(1)).expect("kept");
    assert_eq!(ticket.name, "operations/1");
    assert_eq!(ticket.instance, "main");
    assert_eq!(*ticket.stage.borrow(), Stage::Done(failed("a")));
    ops.finish(OperationId(1), failed("b"), Instant::now());
    ops.finish(OperationId(5), failed("c"), Instant::now());
    assert_eq!(
        *ops.ticket(OperationId(1)).expect("kept").stage.borrow(),
        Stage::Done(failed("a")),
        "a second finish changes nothing"
    );
    assert!(ops.ticket(OperationId(5)).is_none());
}

/// Catches an operation taken up from the state store displacing a live one of the same
/// action as the one new requests join (they would join a run another process began), and a
/// cancelled one never displaced (every new request would open an operation of its own): the
/// live one is kept, the cancelled one is displaced by the next.
#[tokio::test(start_paused = true)]
async fn only_a_cancelled_operation_of_a_key_is_displaced_as_the_one_joined() {
    let mut ops = Ops::new(None);
    let _old = ops.open(OperationId(1), key(1));
    let _taken_up = ops.open(OperationId(2), key(1));
    assert_eq!(
        ops.join(&key(1)).expect("the live one").operation,
        OperationId(1)
    );
    assert!(ops.cancel(OperationId(1)));
    let _new = ops.open(OperationId(3), key(1));
    assert_eq!(
        ops.join(&key(1)).expect("the newest").operation,
        OperationId(3)
    );
    ops.finish(OperationId(1), failed("old"), Instant::now());
    assert_eq!(
        ops.join(&key(1)).expect("still the newest").operation,
        OperationId(3)
    );
    ops.hand_over(OperationId(2));
    ops.hand_over(OperationId(3));
    assert!(ops.join(&key(1)).is_none());
    // The one joined handed over leaves no holder of the key: the operation taken up beside it
    // goes on for its own callers, and a new request opens one of its own.
    let mut again = Ops::new(None);
    let _a = again.open(OperationId(1), key(1));
    let _b = again.open(OperationId(2), key(1));
    again.hand_over(OperationId(1));
    assert!(again.join(&key(1)).is_none());
    assert!(again.is_open(OperationId(2)));
}

/// An operation handed over is followed, not open, and answered when the follower is ended;
/// asking again for its follower gives the one there.
#[tokio::test(start_paused = true)]
async fn a_handed_over_operation_is_followed_until_its_answer_comes() {
    let mut ops = Ops::new(None);
    let (seen, _) = ops.open(OperationId(1), key(1));
    ops.hand_over(OperationId(1));
    ops.hand_over(OperationId(9));
    assert!(!ops.is_open(OperationId(1)));
    assert!(ops.is_followed(OperationId(1)));
    assert_eq!(*seen.borrow(), Stage::Queued);
    let ticket = ops.ticket(OperationId(1)).expect("followed");
    let (again, new) = ops.follower(OperationId(1), key(1));
    assert!(!new, "the follower is there");
    ops.follow_done(OperationId(1), failed("theirs"), Instant::now());
    assert_eq!(*seen.borrow(), Stage::Done(failed("theirs")));
    assert_eq!(*ticket.stage.borrow(), Stage::Done(failed("theirs")));
    assert_eq!(*again.borrow(), Stage::Done(failed("theirs")));
    assert!(!ops.is_followed(OperationId(1)));
    assert!(
        ops.ticket(OperationId(1)).is_some(),
        "kept like an answer of this process"
    );
    ops.follow_done(OperationId(1), failed("twice"), Instant::now());
    assert_eq!(
        *ops.ticket(OperationId(1)).expect("kept").stage.borrow(),
        Stage::Done(failed("theirs"))
    );
}

/// The follower of an operation no one follows yet is new, and told so once.
#[test]
fn a_new_follower_is_reported_new_once() {
    let mut ops = Ops::new(None);
    let (stage, new) = ops.follower(OperationId(3), key(3));
    assert!(new);
    assert_eq!(*stage.borrow(), Stage::Queued);
    assert!(!ops.follower(OperationId(3), key(3)).1);
    assert!(ops.ticket(OperationId(3)).is_some());
    assert!(ops.ticket(OperationId(4)).is_none());
}

/// An answer is forgotten once kept for the retention, the oldest first, and no sooner.
#[tokio::test(start_paused = true)]
async fn answers_are_forgotten_after_the_retention_oldest_first() {
    let mut ops = Ops::new(None);
    let retention = Duration::from_secs(10);
    let start = Instant::now();
    for n in 1..=3_u64 {
        let _ = ops.open(OperationId(n), key(1));
        ops.finish(OperationId(n), failed("x"), start + Duration::from_secs(n));
    }
    ops.prune(start + Duration::from_secs(10), retention);
    assert!(
        ops.ticket(OperationId(1)).is_some(),
        "kept until its retention is out"
    );
    ops.prune(start + Duration::from_secs(11), retention);
    assert!(
        ops.ticket(OperationId(1)).is_none(),
        "kept exactly its retention"
    );
    assert!(ops.ticket(OperationId(2)).is_some());
    ops.prune(start + Duration::from_secs(13), retention);
    assert!(ops.ticket(OperationId(2)).is_none());
    assert!(ops.ticket(OperationId(3)).is_none());
    ops.prune(start + Duration::from_secs(100), retention);
}

/// Catches a request of an action joining its operation after a caller cancelled it: the
/// request would be answered CANCELLED for a cancel it never asked for. It opens an operation
/// of its own, and the cancelled one is not displaced for its callers.
#[test]
fn a_cancelled_operation_is_not_joined() {
    let mut ops = Ops::new(Some(1));
    let key = key(1);
    let first = ops.next_id();
    let (_stage, _cancel) = ops.open(first, key.clone());
    assert!(ops.join(&key).is_some());
    assert!(ops.cancel(first));
    assert!(
        ops.join(&key).is_none(),
        "a cancelled operation takes no new caller"
    );
    assert!(
        ops.ticket(first).is_some(),
        "its own callers still read it by name"
    );
}
