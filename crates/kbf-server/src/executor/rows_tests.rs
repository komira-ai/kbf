//! The executor's rows of the state store: ids, operation rows, answers and the claim of a gone
//! process's operations, over the in-memory store of the state crate.

use std::sync::atomic::Ordering;

use kbf_proto::reapi::ActionResult;
use kbf_state::ManualClock;
use kbf_types::DigestFunction;

use super::*;
use crate::executor::tests::Faulty;

struct Env {
    clock: Arc<ManualClock>,
    store: Arc<Faulty>,
}

fn env() -> Env {
    let clock = Arc::new(ManualClock::default());
    let store = Arc::new(Faulty::new(Arc::clone(&clock)));
    Env { clock, store }
}

impl Env {
    fn rows(&self, instance: &str) -> Rows {
        Rows::new(self.store.clone() as Arc<dyn Store>, instance)
    }

    fn raw(&self, key: &str) -> Option<Vec<u8>> {
        self.store.get(key).expect("a read").map(|row| row.value)
    }

    fn put(&self, key: &str, value: &[u8]) {
        self.store
            .txn(Txn::new().put(key, value.to_vec(), Cond::Any))
            .expect("a write")
            .expect("no conflict");
    }

    fn pass(&self, by: Duration) {
        self.clock.advance(by);
    }
}

fn action(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], 4096 + u64::from(tag)),
    }
}

fn row(tag: u8) -> OpRow {
    let key = action(tag);
    OpRow {
        owner: "ignored".to_owned(),
        instance: key.instance,
        hash: key.action.hash_hex(),
        size: key.action.size_bytes,
        attempt: 1,
        job: format!("k0-{tag}-1"),
        key: Some(format!("cas/b/{tag}-1/aa")),
        whole_machine: true,
    }
}

fn ran(exit_code: i32) -> Finished {
    Finished::Ran(Box::new(ActionResult {
        exit_code,
        ..ActionResult::default()
    }))
}

fn op(n: u64) -> OperationId {
    OperationId(n)
}

const DAY: Duration = Duration::from_secs(86_400);

// ---- ids

/// Ids come from blocks of the shared counter: two rows over one store never give one id, the
/// first is 1, and a process that takes its next block takes it past every other's.
#[test]
fn id_blocks_are_shared_between_two_rows_over_one_store_and_never_zero() {
    let env = env();
    let (a, b) = (env.rows("a"), env.rows("b"));
    assert_eq!(a.next_id().expect("an id"), op(1));
    assert_eq!(b.next_id().expect("an id"), op(1 + ID_BLOCK));
    let mut seen = std::collections::BTreeSet::from([1, 1 + ID_BLOCK]);
    for _ in 0..ID_BLOCK - 1 {
        assert!(
            seen.insert(a.next_id().expect("an id").0),
            "a block id is given once"
        );
    }
    assert_eq!(a.next_id().expect("the next block").0, 1 + 2 * ID_BLOCK);
    assert_eq!(b.next_id().expect("an id"), op(2 + ID_BLOCK));
    assert!(!seen.contains(&0));
    assert_eq!(
        env.raw("meta/next_op"),
        Some((1 + 3 * ID_BLOCK).to_be_bytes().to_vec()),
        "the counter is the next free id, 8 bytes big-endian"
    );
}

/// A counter of an earlier release that reads 0 never gives id 0, one that reads more starts
/// there, and one that is not a number is refused.
#[test]
fn a_counter_that_reads_zero_starts_at_one_and_one_that_is_no_number_is_refused() {
    let env = env();
    env.put("meta/next_op", &0_u64.to_be_bytes());
    assert_eq!(env.rows("a").next_id().expect("an id"), op(1));
    env.put("meta/next_op", &5000_u64.to_be_bytes());
    assert_eq!(env.rows("b").next_id().expect("an id"), op(5000));
    env.put("meta/next_op", b"short");
    let error = env.rows("c").next_id().expect_err("no number");
    assert_eq!(
        error,
        RowsError::Corrupt {
            key: "meta/next_op".to_owned(),
            why: "not an 8-byte number".to_owned()
        }
    );
    assert!(error.to_string().contains("is corrupt"));
}

/// A counter every other writer wins is given up on after a few tries, as a conflict; a store
/// that fails refuses the id as a store error.
#[test]
fn an_id_is_refused_when_other_frontends_win_every_try_or_the_store_fails() {
    let env = env();
    *env.store.race.lock().expect("lock") = Some("meta/next_op".to_owned());
    assert_eq!(
        env.rows("a").next_id().expect_err("always beaten"),
        RowsError::Conflict {
            key: "meta/next_op".to_owned()
        }
    );
    *env.store.race.lock().expect("lock") = None;
    env.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(matches!(env.rows("b").next_id(), Err(RowsError::Store(_))));
    env.store.reads_fail.store(false, Ordering::SeqCst);
    env.store.writes_fail.store(true, Ordering::SeqCst);
    assert!(matches!(env.rows("c").next_id(), Err(RowsError::Store(_))));
}

// ---- the operation rows

/// An operation's row is written new, then replaced by its owner, under its owner's name, as
/// one version byte and JSON that names no url, token or secret; another process cannot write
/// it.
#[test]
fn put_op_writes_the_row_for_its_owner_and_refuses_another_writer() {
    let env = env();
    let (a, b) = (env.rows("a"), env.rows("b"));
    a.put_op(op(9), &row(1)).expect("a new row");
    let stored = a.op(op(9)).expect("read").expect("there");
    assert_eq!(
        stored,
        OpRow {
            owner: "a".to_owned(),
            ..row(1)
        }
    );
    let raw = env.raw(&op_key(op(9))).expect("a value");
    assert_eq!(raw[0], VERSION);
    let json = String::from_utf8(raw[1..].to_vec()).expect("json");
    assert!(!json.contains("sig=") && !json.contains("token"), "{json}");
    assert_eq!(op_key(op(9)), "exec/op/0000000000000009");
    let next = OpRow {
        attempt: 2,
        ..row(1)
    };
    a.put_op(op(9), &next).expect("its owner replaces it");
    assert_eq!(a.op(op(9)).expect("read").expect("there").attempt, 2);
    assert_eq!(
        b.put_op(op(9), &row(1)),
        Err(RowsError::Conflict { key: op_key(op(9)) })
    );
    assert_eq!(a.op(op(9)).expect("read").expect("there").attempt, 2);
}

/// A row this build does not know, or cannot read, is not overwritten as if it were its own.
#[test]
fn put_op_does_not_overwrite_a_row_it_cannot_read_or_does_not_know() {
    let env = env();
    let a = env.rows("a");
    env.put(&op_key(op(1)), &[VERSION + 1, b'{', b'}']);
    assert_eq!(
        a.put_op(op(1), &row(1)),
        Err(RowsError::Conflict { key: op_key(op(1)) })
    );
    env.put(&op_key(op(2)), &[VERSION, b'{']);
    assert!(matches!(
        a.put_op(op(2), &row(1)),
        Err(RowsError::Corrupt { .. })
    ));
    env.put(&op_key(op(3)), &[]);
    assert_eq!(
        a.put_op(op(3), &row(1)),
        Err(RowsError::Corrupt {
            key: op_key(op(3)),
            why: "the row is empty".to_owned()
        })
    );
    env.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(matches!(a.put_op(op(4), &row(1)), Err(RowsError::Store(_))));
    env.store.reads_fail.store(false, Ordering::SeqCst);
    env.store.writes_fail.store(true, Ordering::SeqCst);
    assert!(matches!(a.put_op(op(4), &row(1)), Err(RowsError::Store(_))));
}

/// The row of an operation reads back as written; an unknown version reads as none, an
/// unreadable one is an error, and a row of a later release with more fields still reads.
#[test]
fn op_reads_the_row_and_skips_a_version_it_does_not_know() {
    let env = env();
    let a = env.rows("a");
    assert_eq!(a.op(op(1)), Ok(None));
    env.put(&op_key(op(2)), &[VERSION + 1, 0xff]);
    assert_eq!(a.op(op(2)), Ok(None));
    env.put(&op_key(op(3)), b"\x01nonsense");
    assert!(matches!(a.op(op(3)), Err(RowsError::Corrupt { .. })));
    let mut later = vec![VERSION];
    later.extend(
        serde_json::to_vec(&serde_json::json!({
            "owner": "z", "instance": "main", "hash": "ab".repeat(32), "size": 5,
            "attempt": 0, "job": "j", "a_field_of_a_later_release": [1, 2],
        }))
        .expect("json"),
    );
    env.put(&op_key(op(4)), &later);
    let read = a.op(op(4)).expect("read").expect("there");
    assert_eq!(
        (read.key, read.whole_machine),
        (None, false),
        "a missing field is its default"
    );
    env.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(matches!(a.op(op(4)), Err(RowsError::Store(_))));
}

/// The action an operation row names is read from its hash and size, or refused.
#[test]
fn an_op_row_names_its_action_or_says_why_it_does_not() {
    assert_eq!(row(3).action_key(), Ok(action(3)));
    let bad = OpRow {
        hash: "abc".to_owned(),
        ..row(3)
    };
    assert!(bad.action_key().is_err());
}

// ---- finishing

/// A finished operation's row goes, its answer and action are put under the formats of earlier
/// releases for the retention, and at least ten minutes whatever the retention.
#[test]
fn finish_deletes_the_row_and_keeps_the_answer_for_its_retention_or_ten_minutes() {
    let env = env();
    let a = env.rows("a");
    a.put_op(op(1), &row(1)).expect("row");
    a.put_op(op(2), &row(2)).expect("row");
    let key = action(1);
    let answer = ran(5);
    assert_eq!(
        a.finish(op(1), &key, &answer, Duration::from_secs(1))
            .expect("finished"),
        answer
    );
    assert_eq!(a.op(op(1)), Ok(None));
    assert_eq!(a.done(op(1)), Ok(Some((key.clone(), answer.clone()))));
    assert!(env.raw("done/0000000000000001").is_some());
    a.finish(op(2), &action(2), &answer, DAY).expect("finished");
    env.pass(Duration::from_secs(599));
    assert!(
        a.done(op(1)).expect("read").is_some(),
        "kept at least ten minutes"
    );
    env.pass(Duration::from_secs(2));
    assert_eq!(a.done(op(1)), Ok(None));
    assert_eq!(env.raw("done/0000000000000001"), None);
    assert_eq!(env.raw("doneof/0000000000000001"), None);
    env.pass(DAY - Duration::from_secs(700));
    assert!(
        a.done(op(2)).expect("read").is_some(),
        "kept as long as asked"
    );
    env.pass(Duration::from_secs(200));
    assert_eq!(a.done(op(2)), Ok(None));
}

/// The `doneof/` row is the format earlier releases read: version 1, the size big-endian, the
/// hash, then the instance; the `done/` row is the answer's own.
#[test]
fn the_done_rows_are_in_the_format_of_earlier_releases() {
    let env = env();
    let a = env.rows("a");
    let key = ActionKey {
        instance: "main/é".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [7; 32], 0x0102_0304_0506_0708),
    };
    let answer = ran(0);
    a.finish(op(0x2a), &key, &answer, DONE_FLOOR)
        .expect("finished");
    let mut want = vec![1_u8];
    want.extend([1, 2, 3, 4, 5, 6, 7, 8]);
    want.extend([7_u8; 32]);
    want.extend("main/é".as_bytes());
    assert_eq!(env.raw("doneof/000000000000002a"), Some(want));
    assert_eq!(env.raw("done/000000000000002a"), Some(answer.to_row()));
    assert_eq!(a.done(op(0x2a)), Ok(Some((key, answer))));
}

/// An operation with no row of its own (it was never written, or is gone) finishes all the
/// same; another process's row cannot be finished.
#[test]
fn finish_with_no_row_succeeds_and_with_another_owners_row_conflicts() {
    let env = env();
    let (a, b) = (env.rows("a"), env.rows("b"));
    a.finish(op(1), &action(1), &ran(0), DONE_FLOOR)
        .expect("no row to delete");
    b.put_op(op(2), &row(2)).expect("their row");
    assert_eq!(
        a.finish(op(2), &action(2), &ran(0), DONE_FLOOR),
        Err(RowsError::Conflict { key: op_key(op(2)) })
    );
    assert!(b.op(op(2)).expect("read").is_some(), "their row stays");
    assert_eq!(a.done(op(2)), Ok(None), "and no answer is written");
}

/// An answer another frontend wrote first stands: it is returned, this process's is dropped,
/// and the row goes.
#[test]
fn finish_keeps_an_answer_another_frontend_wrote_first() {
    let env = env();
    let (a, b) = (env.rows("a"), env.rows("b"));
    b.finish(op(1), &action(1), &ran(11), DONE_FLOOR)
        .expect("first");
    a.put_op(op(1), &row(1)).expect("row");
    let kept = a
        .finish(op(1), &action(1), &ran(22), DONE_FLOOR)
        .expect("the earlier answer");
    assert_eq!(kept, ran(11));
    assert_eq!(a.op(op(1)), Ok(None), "the row is deleted");
    assert_eq!(a.done(op(1)), Ok(Some((action(1), ran(11)))));
}

/// An answer that is half written (its action row missing) cannot be returned, and says so;
/// a store that fails while the row is deleted fails the finish.
#[test]
fn finish_refuses_a_half_written_answer_and_a_store_that_fails() {
    let env = env();
    let a = env.rows("a");
    env.put("done/0000000000000001", &ran(1).to_row());
    let error = a
        .finish(op(1), &action(1), &ran(2), DONE_FLOOR)
        .expect_err("half written");
    assert!(matches!(error, RowsError::Corrupt { .. }), "{error:?}");
    env.put("done/0000000000000002", &ran(1).to_row());
    env.put("doneof/0000000000000002", &[1; 0]);
    a.put_op(op(3), &row(3)).expect("row");
    env.put("done/0000000000000003", &ran(1).to_row());
    env.put("doneof/0000000000000003", &{
        let mut v = vec![1_u8];
        v.extend([0; 8]);
        v.extend([3; 32]);
        v.extend(b"main");
        v
    });
    env.store.fail_nth_write(2);
    assert!(
        matches!(
            a.finish(op(3), &action(3), &ran(2), DONE_FLOOR),
            Err(RowsError::Store(_))
        ),
        "the delete of the row after the earlier answer"
    );
    a.put_op(op(4), &row(4)).expect("row");
    env.store.writes_fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        a.finish(op(4), &action(4), &ran(2), DONE_FLOOR),
        Err(RowsError::Store(_))
    ));
    env.store.writes_fail.store(false, Ordering::SeqCst);
    env.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        a.finish(op(4), &action(4), &ran(2), DONE_FLOOR),
        Err(RowsError::Store(_))
    ));
}

/// A row that changed between the read and the write loses the finish as a conflict on the
/// row itself, and writes no answer.
#[test]
fn finish_loses_to_a_write_of_its_row_in_between() {
    let env = env();
    let a = env.rows("a");
    a.put_op(op(1), &row(1)).expect("row");
    *env.store.race.lock().expect("lock") = Some(op_key(op(1)));
    assert_eq!(
        a.finish(op(1), &action(1), &ran(0), DONE_FLOOR),
        Err(RowsError::Conflict { key: op_key(op(1)) })
    );
    *env.store.race.lock().expect("lock") = None;
    assert_eq!(a.done(op(1)), Ok(None));
}

// ---- reading an answer

/// An answer is there only with both its rows, and a row that is not what it says is an error
/// naming it.
#[test]
fn done_needs_both_rows_and_names_the_row_that_is_corrupt() {
    let env = env();
    let a = env.rows("a");
    assert_eq!(a.done(op(1)), Ok(None));
    env.put("done/0000000000000001", &ran(1).to_row());
    assert_eq!(a.done(op(1)), Ok(None), "no action row");
    let good = {
        let mut v = vec![1_u8];
        v.extend(9_u64.to_be_bytes());
        v.extend([5; 32]);
        v.extend(b"main");
        v
    };
    env.put("doneof/0000000000000001", &good);
    assert!(a.done(op(1)).expect("read").is_some());
    let mut other = good.clone();
    other[0] = 2;
    let cut = &good[..20];
    let mut not_utf8 = good[..41].to_vec();
    not_utf8.push(0xff);
    for (what, bytes) in [
        ("version", other),
        ("cut", cut.to_vec()),
        ("short", vec![1, 0, 0]),
        ("utf8", not_utf8),
    ] {
        env.put("doneof/0000000000000001", &bytes);
        let error = a.done(op(1)).expect_err(what);
        assert!(
            matches!(&error, RowsError::Corrupt { key, .. } if key == "doneof/0000000000000001"),
            "{what}: {error:?}"
        );
    }
    env.put("doneof/0000000000000001", &good);
    env.put("done/0000000000000001", &[0xff, 0xff]);
    assert!(
        matches!(a.done(op(1)), Err(RowsError::Corrupt { key, .. }) if key == "done/0000000000000001")
    );
    env.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(matches!(a.done(op(1)), Err(RowsError::Store(_))));
}

// ---- liveness and orphans

/// A process lives while its row does: renewed with the time to live, it expires when it is
/// not renewed, and is deleted at once on leaving (twice is no harm).
#[test]
fn a_liveness_row_expires_unless_renewed_and_goes_when_its_process_leaves() {
    let env = env();
    let a = env.rows("a");
    let live = Duration::from_secs(20);
    a.beat(live).expect("beat");
    assert!(env.raw("exec/live/a").is_some());
    env.pass(Duration::from_secs(15));
    a.beat(live).expect("renewed");
    env.pass(Duration::from_secs(15));
    assert!(env.raw("exec/live/a").is_some(), "renewed 15 s ago");
    env.pass(Duration::from_secs(6));
    assert!(
        env.raw("exec/live/a").is_none(),
        "expired 20 s after its last renewal"
    );
    a.beat(live).expect("beat");
    a.leave().expect("left");
    assert!(env.raw("exec/live/a").is_none());
    a.leave().expect("left again");
    env.store.writes_fail.store(true, Ordering::SeqCst);
    assert!(matches!(a.beat(live), Err(RowsError::Store(_))));
    assert!(matches!(a.leave(), Err(RowsError::Store(_))));
}

/// The unfinished operations of gone processes are listed, with the version to claim them at;
/// a live owner's, this process's own, rows of a later release's version (counted) and corrupt
/// rows, and keys that are no operation's, are not.
#[test]
fn orphans_skip_live_owners_own_rows_unknown_versions_and_corrupt_rows() {
    let env = env();
    let (me, gone, alive) = (env.rows("me"), env.rows("gone"), env.rows("alive"));
    me.put_op(op(1), &row(1)).expect("own");
    gone.put_op(op(2), &row(2)).expect("gone");
    gone.put_op(op(3), &row(3))
        .expect("gone, the owner looked up once");
    alive.put_op(op(4), &row(4)).expect("alive");
    alive.beat(Duration::from_secs(20)).expect("alive");
    env.put(&op_key(op(5)), &[VERSION + 1, 1, 2, 3]);
    env.put(&op_key(op(6)), b"\x01{broken");
    env.put("exec/op/short", b"\x01{}");
    env.put("exec/op/zzzzzzzzzzzzzzzz", b"\x01{}");
    let orphans = me.orphans().expect("a scan");
    let named: Vec<(u64, String, u32)> = orphans
        .iter()
        .map(|o| (o.operation.0, o.row.owner.clone(), o.row.attempt))
        .collect();
    assert_eq!(
        named,
        [(2, "gone".to_owned(), 1), (3, "gone".to_owned(), 1)]
    );
    assert_eq!(me.unknown_rows(), 1);
    me.orphans().expect("a scan");
    assert_eq!(me.unknown_rows(), 2, "counted at each scan");
    env.pass(Duration::from_secs(21));
    assert_eq!(
        me.orphans().expect("a scan").len(),
        3,
        "an owner that stopped beating is gone"
    );
    env.store.reads_fail.store(true, Ordering::SeqCst);
    assert!(matches!(me.orphans(), Err(RowsError::Store(_))));
}

/// An orphan is claimed by one process only: the next claimer of the same version loses, and
/// the row is the winner's, whose copy of the fields it is.
#[test]
fn claim_wins_once_against_a_racer() {
    let env = env();
    let (gone, b, c) = (env.rows("gone"), env.rows("b"), env.rows("c"));
    gone.put_op(op(1), &row(1)).expect("row");
    let for_b = b.orphans().expect("scan");
    let for_c = c.orphans().expect("scan");
    let won = b.claim(&for_b[0]).expect("the first claim wins");
    assert_eq!(
        won,
        OpRow {
            owner: "b".to_owned(),
            ..row(1)
        }
    );
    assert_eq!(
        c.claim(&for_c[0]),
        Err(RowsError::Conflict { key: op_key(op(1)) })
    );
    assert_eq!(b.op(op(1)).expect("read").expect("there").owner, "b");
    assert_eq!(
        c.orphans().expect("scan").len(),
        1,
        "b has not beaten: its row is an orphan too"
    );
    b.beat(Duration::from_secs(20)).expect("beat");
    assert!(
        c.orphans().expect("scan").is_empty(),
        "b owns it, and b is live"
    );
}

/// A claim fails when the owner's liveness row is back, and when the store fails; the row is
/// then the owner's still.
#[test]
fn claim_is_refused_when_the_owner_is_live_again_or_the_store_fails() {
    let env = env();
    let (gone, b) = (env.rows("gone"), env.rows("b"));
    gone.put_op(op(1), &row(1)).expect("row");
    let orphan = b.orphans().expect("scan").remove(0);
    gone.beat(Duration::from_secs(20)).expect("it is back");
    assert_eq!(
        b.claim(&orphan),
        Err(RowsError::Conflict {
            key: "exec/live/gone".to_owned()
        })
    );
    assert_eq!(b.op(op(1)).expect("read").expect("there").owner, "gone");
    gone.leave().expect("gone again");
    env.store.writes_fail.store(true, Ordering::SeqCst);
    assert!(matches!(b.claim(&orphan), Err(RowsError::Store(_))));
}

/// A store that fails at any read of a call fails the call, wherever in it the read is.
#[test]
fn a_read_that_fails_part_way_through_a_call_fails_the_call() {
    let env = env();
    let a = env.rows("a");
    a.put_op(op(1), &row(1)).expect("row");
    env.store.fail_nth_read(1);
    assert!(
        matches!(a.put_op(op(1), &row(1)), Err(RowsError::Store(_))),
        "the ownership read"
    );
    env.store.fail_nth_read(1);
    assert!(
        matches!(
            a.finish(op(1), &action(1), &ran(0), DONE_FLOOR),
            Err(RowsError::Store(_))
        ),
        "the ownership read"
    );
    a.put_op(op(2), &row(2)).expect("row");
    env.rows("b")
        .finish(op(3), &action(3), &ran(0), DONE_FLOOR)
        .expect("their answer");
    a.put_op(op(3), &row(3)).expect("row");
    // The row and its ownership, then the answer's reads after the conflict.
    env.store.fail_nth_read(2);
    assert!(
        matches!(
            a.finish(op(3), &action(3), &ran(1), DONE_FLOOR),
            Err(RowsError::Store(_))
        ),
        "the read of the answer that was there first"
    );
    env.store.fail_nth_read(2);
    assert!(
        matches!(a.done(op(3)), Err(RowsError::Store(_))),
        "the second row of an answer"
    );
    gone_orphan(&env);
    env.store.fail_nth_read(2);
    assert!(
        matches!(a.orphans(), Err(RowsError::Store(_))),
        "the liveness read"
    );
}

fn gone_orphan(env: &Env) {
    env.rows("gone").put_op(op(9), &row(9)).expect("row");
}

/// Catches the answer read before the row: an operation whose owner finishes it between the
/// two reads of a look (the row deleted and the answer put in one transaction) would read as
/// neither open nor done, and a follower would answer its callers UNAVAILABLE for an operation
/// that ran. The row is read first, so the look finds the answer.
#[test]
fn a_finish_between_the_reads_of_a_look_is_seen_done() {
    let env = env();
    let a = env.rows("p-1");
    a.put_op(op(1), &row(1)).expect("written");
    assert_eq!(
        a.look(op(1)).expect("a look"),
        Look::Open(OpRow {
            owner: "p-1".to_owned(),
            ..row(1)
        })
    );
    let owner = env.rows("p-1");
    *env.store.before_read.lock().expect("unpoisoned") = Some((
        OP_PREFIX.to_owned(),
        Box::new(move || {
            owner
                .finish(op(1), &action(1), &ran(0), DONE_FLOOR)
                .expect("finished by its owner");
        }),
    ));
    let follower = env.rows("p-2");
    assert_eq!(
        follower.look(op(1)).expect("a look"),
        Look::Done(action(1), ran(0))
    );
    assert_eq!(follower.look(op(9)).expect("a look"), Look::Gone);
}
