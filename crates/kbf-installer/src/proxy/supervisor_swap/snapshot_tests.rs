//! `SwapSupervisor::snapshot`: the slot that serves, the commit its child runs and the newest
//! update's record, as the swap-state endpoint reads them. The supervisor is the one of the
//! blocked tests, over the rig, with its swap thread accepted and never run, so a test sets
//! the record the swap would have written.

use super::Snapshot;
use super::blocked_tests::{over, spec};
use crate::proxy::kinds::Supervisor;
use crate::proxy::model::{Phase, Slot};
use crate::testkit::{A, B, c};

/// Asserts that, with no update on record, the slot is the one the start-up adopted, the commit
/// is the one recorded for the process in it (not read from an update's record), and there is
/// no update.
#[test]
fn with_no_update_the_slot_is_the_adopted_one_and_the_commit_is_its_record() {
    let (sup, _dir) = over("snap-idle", |_, _| {});

    let got = sup.snapshot();

    assert_eq!(got.slot, Some(Slot::A));
    assert_eq!(got.running, Some(c(A)));
    assert_eq!(got.intent, None);
}

/// Asserts that the commit is read from the slot that serves: when the adopted slot is B, with
/// a process started from release `B`, that is the commit, though slot A holds a process of `A`.
#[test]
fn the_commit_is_the_record_of_the_slot_that_serves() {
    let (sup, _dir) = over("snap-b", |rig, config| {
        config.active = Slot::B;
        rig.occupy(Slot::B);
    });

    let got = sup.snapshot();

    assert_eq!(got.slot, Some(Slot::B));
    assert_eq!(got.running, Some(c(B)));
}

/// Asserts that while the start-up hold is in force no frontend child is known to serve, so
/// there is no slot and no commit.
#[test]
fn while_the_startup_hold_is_in_force_no_slot_and_no_commit_are_named() {
    let (sup, _dir) = over("snap-hold", |_, config| {
        config.startup_hold = Some("no frontend child is running".to_owned());
    });

    assert_eq!(sup.snapshot(), Snapshot::default());
}

/// Asserts that the snapshot follows the swap: an update just started is served by the old
/// child (slot A, release `A`), with the record of the update as the swap wrote it (its start
/// time is set, and is the time of its first phase); from the switch the new child serves
/// (slot B, release `B`).
#[test]
fn an_open_update_serves_from_the_old_child_until_the_switch_and_the_new_one_after() {
    let (mut sup, _dir) = over("snap-open", |_, _| {});
    sup.start(&spec("u1")).expect("accepted");

    let before = sup.snapshot();

    assert_eq!((before.slot, before.running), (Some(Slot::A), Some(c(A))));
    let intent = before.intent.expect("the update is shown");
    assert_eq!(intent.update_id, "u1");
    assert_eq!(intent.phase, Phase::Staged);
    assert_ne!(intent.started_at, 0);
    assert_eq!(intent.started_at, intent.phase_started);

    sup.shared.lock().intent.as_mut().expect("the intent").phase = Phase::Switched;

    let after = sup.snapshot();

    assert_eq!((after.slot, after.running), (Some(Slot::B), Some(c(B))));
}

/// Asserts that an ended update is not read as an open one: once the swap thread is over, the
/// slot is the one the swap left (B, after a `done`), and the record is the ended update's.
#[test]
fn an_ended_update_serves_where_the_swap_left_the_slot() {
    let (mut sup, _dir) = over("snap-ended", |_, _| {});
    sup.start(&spec("u1")).expect("accepted");
    sup.shared.lock().intent.as_mut().expect("the intent").phase = Phase::Done;
    sup.shared.lock().active = Slot::B;
    sup.shared.close();

    let got = sup.snapshot();

    assert_eq!((got.slot, got.running), (Some(Slot::B), Some(c(B))));
    assert_eq!(got.intent.map(|intent| intent.phase), Some(Phase::Done));
}

/// Asserts that the hold hides only the slot and the commit: they are unknown, the record of
/// the update is still shown.
#[test]
fn the_update_is_shown_when_the_hold_hides_the_slot() {
    let (mut sup, _dir) = over("snap-hold-update", |_, config| {
        config.startup_hold = Some("no frontend child is running".to_owned());
    });
    sup.start(&spec("u1")).expect("accepted");

    let got = sup.snapshot();

    assert_eq!((got.slot, got.running), (None, None));
    assert_eq!(got.intent.map(|i| i.update_id), Some("u1".to_owned()));
}
