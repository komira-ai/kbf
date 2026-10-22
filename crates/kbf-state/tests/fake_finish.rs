//! The fake's two-step commit (`prepare` then `finish`) and its ungrouped-sync flaw, on
//! the answers the conformance suite never asks for: a transaction that loses its
//! precondition between the two steps, and a conflict under a store that syncs per
//! transaction.

use std::sync::Arc;

use kbf_state::fake::{FAKE_SYNC_COST, Fake, Flaw};
use kbf_state::{Clock, Cond, ManualClock, Store, StoreTime, Txn};

fn fake(flaw: Flaw) -> (Arc<ManualClock>, Fake) {
    let clock = Arc::new(ManualClock::default());
    (Arc::clone(&clock), Fake::with_flaw(clock, flaw))
}

// Catches `finish` writing a prepared transaction whose precondition another writer
// broke after `prepare`: it must answer with the conflict, leave the other writer's row,
// and pay no sync for a write that did not happen.
#[test]
fn finish_answers_the_conflict_of_a_precondition_lost_since_prepare() {
    let (clock, store) = fake(Flaw::None);
    let deferred = store.prepare(Txn::new().put("k", "late", Cond::Absent));
    store
        .txn(Txn::new().put("k", "first", Cond::Any))
        .expect("store")
        .expect("commits");
    let synced = clock.now();
    let conflict = store.finish(deferred).expect_err("k exists now");
    assert_eq!((conflict.index, conflict.key.as_str()), (0, "k"));
    assert_eq!(
        store.get("k").expect("store").expect("row").value,
        b"first",
        "the lost write must not land"
    );
    assert_eq!(clock.now(), synced, "a conflict pays no sync");
}

// Catches an ungrouped store that syncs after a transaction that did not apply: only a
// commit costs store time.
#[test]
fn an_ungrouped_store_syncs_for_a_commit_and_not_for_a_conflict() {
    let (clock, store) = fake(Flaw::Ungrouped);
    store
        .txn(Txn::new().put("k", "v", Cond::Absent))
        .expect("store")
        .expect("commits");
    let after_commit = clock.now();
    assert_eq!(
        after_commit,
        StoreTime::ZERO.saturating_add(FAKE_SYNC_COST),
        "one sync for the commit"
    );
    store
        .txn(Txn::new().put("k", "w", Cond::Absent))
        .expect("store")
        .expect_err("k exists");
    assert_eq!(clock.now(), after_commit, "a conflict pays no sync");
}

// Catches a delete of a key that is not there writing a feed row or taking a revision: it
// commits, changes nothing, and the next write gets the next revision.
#[test]
fn deleting_a_key_that_is_not_there_commits_and_writes_nothing() {
    let (_clock, store) = fake(Flaw::None);
    let first = store
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("store")
        .expect("commits");
    store
        .txn(Txn::new().delete("absent", Cond::Any))
        .expect("store")
        .expect("commits");
    let rows = store
        .changes(kbf_state::Revision::ZERO, 10)
        .expect("feed")
        .rows;
    assert_eq!(rows.len(), 1, "only the put is in the feed: {rows:?}");
    assert_eq!(rows[0].revision, first.revision);
}
