//! The host moving under the migration: a verb of the old tools, or another run of this
//! verb, that lands after the migration looked at the old layout and before it holds its
//! locks. The probe is where the tests play it ([`runner_between`]).

use std::fs;

use serde_json::json;

use super::journal::{Journal, Phase};
use super::rig::*;
use super::*;
use crate::helper::memory::Memory;
use crate::state::{Locked, State};
use crate::testkit::{A, B, C};

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

/// An apply in which `between` lands after the migration's look and before its locks.
fn apply_with(rig: &mut Rig, between: impl Fn() + 'static) -> Result<Report, MigrateError> {
    let runner = runner_between(between);
    run_with(rig, &runner, &never(), &NoFrontend, Mode::Apply)
}

/// The installer's tree was not made and no service was touched.
fn assert_nothing_migrated(rig: &Rig) {
    assert!(!rig.new_state_dir().exists(), "no state directory");
    assert!(!rig.new_tree().exists(), "no tree");
    assert!(rig.events.borrow().is_empty() && rig.host.log.is_empty());
    assert_eq!(
        fs::read_to_string(rig.at("etc/systemd/system/kbf-daemon.service")).expect("unit"),
        OLD_UNIT,
        "the unit is as it was"
    );
}

/// The installer's own state, as its verbs leave it after a migration: `current` is `C`.
fn installer_moved_on(layout: &Layout, state_dir: &std::path::Path) {
    fs::create_dir_all(state_dir).expect("the installer's directory");
    let state = State {
        current: Some(commit(C)),
        ..State::default()
    };
    Locked::acquire(layout)
        .expect("lock")
        .write(&state)
        .expect("the installer's state");
}

/// Catches a migration that adopts what it read before it held the locks: the old tool's
/// swap back lands after the look (the state and links name `A`, and `A` was probed) and
/// before the locks, and the migration would write `A` as the installer's `current`
/// while the old tools say `B`. It is refused (exit 5) with nothing migrated and the
/// host as the swap left it; a run again adopts what is there. The same for `previous`
/// alone.
///
/// Mutants seen red: the writes made from the first look's state; the comparison of the
/// two looks removed.
#[test]
fn an_old_swap_after_the_look_is_refused_and_a_run_again_adopts_the_new_state() {
    let mut rig = Rig::linux("swap-between");
    let hand = rig.old_hand();
    let error = apply_with(&mut rig, move || hand.swap_back()).expect_err("refused");
    assert!(
        matches!(&error, MigrateError::Changed(why) if why.contains(&format!("current was {A} and is now {B}"))),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);
    assert_nothing_migrated(&rig);

    let report = run_migrate(&mut rig, &never(), Mode::Apply).expect("a run again");
    assert_eq!(report.outcome, Outcome::Migrated);
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(
        (&state["current"], &state["previous"]),
        (&json!(B), &json!(A))
    );

    let mut rig = Rig::linux("previous-between");
    let hand = rig.old_hand();
    let error = apply_with(&mut rig, move || {
        hand.state(Some(A), Some(C), None, None);
        hand.link("previous", C);
    })
    .expect_err("refused");
    assert!(
        matches!(&error, MigrateError::Changed(why) if why.contains(&format!("previous was {B} and is now {C}"))),
        "{error}"
    );
    assert_nothing_migrated(&rig);
}

/// Catches a swap put on trial in the window: the old state has a `pending_confirm` the
/// first look did not see, with the same `current` and `previous`, so only the check made
/// under the locks can refuse it (exit 5, naming the release).
///
/// Mutant seen red: the `pending_confirm` check not made under the locks.
#[test]
fn a_swap_put_on_trial_after_the_look_still_refuses_the_migration() {
    let mut rig = Rig::linux("pending-between");
    let hand = rig.old_hand();
    let error = apply_with(&mut rig, move || {
        hand.state(Some(A), Some(B), None, Some(B))
    })
    .expect_err("refused");
    assert!(
        matches!(error, MigrateError::Pending { ref commit } if commit == B),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);
    assert_nothing_migrated(&rig);
}

/// Catches a link moved in the window without its state (an old verb caught half way): the
/// state still names `A` as `current`, the link names `B`, and the release opened and
/// probed is `A`. The links are judged again under the locks (exit 5).
///
/// Mutant seen red: `check_links` not made under the locks.
#[test]
fn a_link_moved_after_the_look_without_its_state_is_refused() {
    let mut rig = Rig::linux("link-between");
    let hand = rig.old_hand();
    let error = apply_with(&mut rig, move || hand.link("current", B)).expect_err("refused");
    assert!(
        matches!(&error, MigrateError::Disagree(why) if why.contains("state.json says current is")),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);
    assert_nothing_migrated(&rig);
}

/// Catches a replay floor seeded from a memory the old helper has since moved past: it
/// acts on order 50 in the window, and the floor written to `state.json` and to
/// `helper.json` must be 50, not the 42 the first look read, or the installer would
/// act on that order again.
///
/// Mutant seen red: the floor seeded from the first look.
#[test]
fn the_floor_is_the_one_seeded_from_the_memory_read_under_the_locks() {
    let mut rig = Rig::linux("floor-between");
    let hand = rig.old_hand();
    let acted = Memory {
        acked_epoch: 3,
        acked_order_id: 50,
        ..Memory::default()
    };
    let report = apply_with(&mut rig, move || hand.memory(&acted)).expect("migrate");
    assert_eq!(report.outcome, Outcome::Migrated);

    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(
        state["floor"],
        json!({"epoch": 3, "order_id": 50, "generation": 17})
    );
    assert_eq!(state["last_acked_order_id"], json!(50));
    let memory = rig.json(&rig.new_state_dir().join("helper.json"));
    assert_eq!(memory["acked_order_id"], json!(50));
    assert_eq!(memory["frontend_order_id"], json!(50));
}

/// Catches a migration run twice at once: the second waits on the locks while the first
/// finishes (the journal says `done`) and the installer moves on to `C`. The second must
/// report a finished migration and leave the installer's `state.json` alone; run from
/// the look it took before waiting, it would write `A` over `C` and stop the node again.
///
/// Mutant seen red: the journal not read again under the locks.
#[test]
fn a_run_that_finished_while_this_one_waited_is_not_run_again() {
    let mut rig = Rig::linux("finished-between");
    let (layout, dir) = (rig.layout.clone(), rig.new_state_dir());
    let report = apply_with(&mut rig, move || {
        installer_moved_on(&layout, &dir);
        let mut journal = Journal::new(Vec::new(), None);
        journal.phase = Phase::Done;
        let bytes = String::from_utf8(journal.render()).expect("text");
        put(&dir.join("migration.json"), &bytes, 0o600);
    })
    .expect("a finished migration is not an error");

    assert_eq!(report.outcome, Outcome::AlreadyMigrated);
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(state["current"], json!(C), "the installer's state is kept");
    assert!(rig.events.borrow().is_empty(), "no service was touched");
}

/// Catches the installer's state appearing in the window with no migration behind it: the
/// first look saw none, and the run is refused as the first look would have refused it
/// (exit 5) rather than overwriting it with the old layout's releases.
///
/// Mutant seen red: the conflict not judged again under the locks.
#[test]
fn an_installer_state_that_appeared_after_the_look_is_not_overwritten() {
    let mut rig = Rig::linux("conflict-between");
    let (layout, dir) = (rig.layout.clone(), rig.new_state_dir());
    let error =
        apply_with(&mut rig, move || installer_moved_on(&layout, &dir)).expect_err("refused");
    assert!(matches!(error, MigrateError::Conflict(_)), "{error}");
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(state["current"], json!(C), "the installer's state is kept");
    assert!(rig.events.borrow().is_empty(), "no service was touched");
}

/// Catches an old layout that is gone in the window (someone removed the old state): it
/// is not "nothing to migrate" for a run that already probed releases to adopt, but a
/// change to run again from (exit 5).
#[test]
fn an_old_state_that_went_after_the_look_is_refused_as_a_change() {
    let mut rig = Rig::linux("gone-between");
    let dir = rig.at("var/lib/kbf-deploy");
    let error = apply_with(&mut rig, move || {
        fs::remove_file(dir.join("state.json")).expect("rm");
        fs::remove_file(dir.join("helper.json")).expect("rm");
    })
    .expect_err("refused");
    assert!(
        matches!(&error, MigrateError::Changed(why) if why.contains("gone")),
        "{error}"
    );
    assert_eq!(error.exit().code(), 5);
    assert_nothing_migrated(&rig);
}
