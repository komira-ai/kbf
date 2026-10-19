//! The lock beside a state file is free again the moment what held it is dropped (the
//! store, a demoted store, a `StoreLock`), whatever other threads of the process are
//! doing.
//!
//! An `flock` belongs to the open file description, which lives until every copy of the
//! descriptor is closed, and a process forked while the lock is held (any other test of
//! a binary starting a program) keeps a copy until it execs. A lock released only by
//! closing its file is then still held after the drop, and the next open is refused.
//! Each child here is held before exec until the lock has been taken again, so the
//! interleaving is the failing one every time.

mod common;

use common::{open_manual, scratch};
use kbf_state::Store;
use kbf_state::sqlite::{Options, StoreLock};
use kbf_testkit::forked::ForkedChild;

// Catches a lock the store's drop releases only by closing its descriptor: the restart
// of a frontend in a process that also spawns children (a test binary) is refused with
// `Locked`. Mutant: the `Drop` of `LockFile` removed.
#[test]
fn a_child_forked_while_the_store_was_open_does_not_keep_the_file_locked_after_the_drop() {
    let dir = scratch("lock-forked-drop");
    let (first, _) = open_manual(&dir, Options::default());
    let child = ForkedChild::before_exec();
    drop(first);
    let (second, _) = open_manual(&dir, Options::default());
    assert_eq!(second.epoch(), 2, "the file is opened again after the drop");
    drop(second);
    child.finish();
}

// Catches a demotion (the handover to the next frontend) that gives up the lock only
// by closing it: the successor's `StoreLock`, or its open, is refused while any child
// forked in the meantime has not exec'd. Mutant: the `Drop` of `LockFile` removed.
#[test]
fn a_demoted_store_gives_up_the_lock_though_a_forked_child_has_a_copy() {
    let dir = scratch("lock-forked-demote");
    let db = dir.join("state.db");
    let (store, _) = open_manual(&dir, Options::default());
    let child = ForkedChild::before_exec();
    store.demote().expect("demote");
    let taken = StoreLock::take(&db).expect("the lock after the store was demoted");
    drop(taken);
    child.finish();
}

// Catches a `StoreLock` (what a restore holds) that is released only by the close, and
// the answer of `is_held` after it. Mutant: the `Drop` of `LockFile` removed.
#[test]
fn a_store_lock_is_free_after_its_drop_though_a_forked_child_has_a_copy() {
    let dir = scratch("lock-forked-store-lock");
    let db = dir.join("state.db");
    let held = StoreLock::take(&db).expect("take");
    assert!(StoreLock::is_held(&db).expect("probe"), "a held lock");
    let child = ForkedChild::before_exec();
    drop(held);
    assert!(
        !StoreLock::is_held(&db).expect("probe"),
        "the lock outlived the drop"
    );
    let again = StoreLock::take(&db).expect("take again, with the forked child still alive");
    drop(again);
    child.finish();
}
