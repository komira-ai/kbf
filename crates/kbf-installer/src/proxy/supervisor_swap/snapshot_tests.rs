//! `SwapSupervisor::snapshot`: the slot that serves, the commit its child runs and the newest
//! update's record, as the swap-state endpoint reads them. The supervisor is the one of the
//! blocked tests, over the rig, with its swap thread accepted and never run, so a test sets
//! the record the swap would have written.
//!
//! Each "Mutants it catches" line names a change to the code that the test is written to turn
//! red. They are the reason the test exists, not a record that anyone ran them.

use super::Snapshot;
use super::blocked_tests::{over, spec};
use crate::proxy::kinds::Supervisor;
use crate::proxy::model::{Phase, Slot};
use crate::testkit::{A, B, c};

/// Catches a snapshot that names a slot or a commit it does not know: with no update on record
/// the slot is the one the start-up adopted, the commit is the one recorded for the process in
/// it, and there is no update.
///
/// Mutants it catches: the commit read from the update's record only (it would be `None`); the
/// slot not the adopted one.
#[test]
fn with_no_update_the_slot_is_the_adopted_one_and_the_commit_is_its_record() {
    let (sup, _dir) = over("snap-idle", |_, _| {});

    let got = sup.snapshot();

    assert_eq!(got.slot, Some(Slot::A));
    assert_eq!(got.running, Some(c(A)));
    assert_eq!(got.intent, None);
}

/// Catches a commit read from a fixed slot: when the adopted slot is B, with a process started
/// from release `B`, that is the commit, though slot A holds a process of `A`.
///
/// Mutant it catches: the record of slot A asked whichever slot serves.
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

/// Catches a guess given out as a fact: while the start-up hold is in force no frontend child
/// is known to serve, so there is no slot and no commit.
///
/// Mutant it catches: the hold not looked at (slot `a`, commit `A`).
#[test]
fn while_the_startup_hold_is_in_force_no_slot_and_no_commit_are_named() {
    let (sup, _dir) = over("snap-hold", |_, config| {
        config.startup_hold = Some("no frontend child is running".to_owned());
    });

    assert_eq!(sup.snapshot(), Snapshot::default());
}

/// Catches a snapshot that does not follow the swap: an update just started is served by the
/// old child (slot A, release `A`), with the record of the update as the swap wrote it (its
/// start time is set, and is the time of its first phase); from the switch the new child serves
/// (slot B, release `B`).
///
/// Mutants it catches: the new child named before the switch, or still named after it; the
/// start time not recorded; the record not shown.
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

/// Catches an ended update read as an open one: once the swap thread is over, the slot is the
/// one the swap left (B, after a `done`), and the record is the ended update's.
///
/// Mutants it catches: the slot taken from the record's phase as for an open update; the
/// record dropped when the update ends.
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

/// Catches a hold that hides the update too: the slot and the commit are unknown, the record of
/// the update is still shown.
///
/// Mutant it catches: the whole snapshot emptied under the hold.
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
