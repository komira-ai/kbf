//! Each step of the slot clean is needed, on a model Mac, on every platform.
//! `tests/slot_clean.rs` has the real run (`#[ignore]`, root, macOS).
//!
//! The freshness test of `tests/slot_clean.rs` claims that leaving any step out of the
//! clean leaves that step's rows for lease B to find. The option is
//! [`Skipped`]/[`WithSkipped`] (`src/slots/clean.rs`): one step left out, the check honest
//! or blind. Here the clean runs through it over a model Mac whose residue is the rows of
//! [`support::slot_rows::COVERS`], each removed by its step; there is no recreate and no
//! retry, so a pass that leaves a row ends dirty.
//!
//! What this proves, and what it does not:
//! - It proves the option and the table: the option leaves out exactly the step it names;
//!   a blind check lets the residue through as a clean (so it would reach lease B),
//!   exactly the rows of that step; an honest check never reports a clean while a row is
//!   left (the slot is dirty, never clean); every row of the freshness test is under a
//!   step or listed as removed by none.
//! - It does not prove the table. The model removes a row because the table says its
//!   step does, so these tests cannot catch a table that is wrong about the real steps.
//!   That is `tests/slot_clean/steps_needed.rs`, which runs the same table against the
//!   real steps on a Mac.

mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;

use kbf_driver_native::slots::clean::{
    self, Check, Left, Os, Outcome, STEPS, Skipped, Step, WithSkipped,
};
use support::slot_rows::{
    CORE, COVERS, ROWS, UNCOVERED, UNROWED, Unplanted, classify_unplanted, covered_by,
};

/// A row left by the lease, and the step that removes it.
type Residue = (&'static str, Step);

/// A Mac with every row of [`COVERS`] planted. Each step removes the residue the table
/// gives it; the look names what is left, under the step that should have removed it;
/// there is no recreate: a pass that leaves a row leaves the user dirty.
struct Mac {
    residue: RefCell<BTreeSet<Residue>>,
    ran: RefCell<Vec<Step>>,
}

impl Mac {
    fn planted() -> Self {
        let residue = COVERS
            .iter()
            .flat_map(|(step, rows)| rows.iter().map(|row| (*row, *step)))
            .collect();
        Self {
            residue: RefCell::new(residue),
            ran: RefCell::default(),
        }
    }

    /// The rows still there, as the freshness test would see them in lease B.
    fn rows(&self) -> BTreeSet<&'static str> {
        self.residue.borrow().iter().map(|(row, _)| *row).collect()
    }

    fn removing(&self, steps: &[Step]) {
        self.residue
            .borrow_mut()
            .retain(|(_, owner)| !steps.contains(owner));
    }
}

impl Os for Mac {
    fn run(&self, step: Step) -> Result<(), String> {
        self.ran.borrow_mut().push(step);
        self.removing(&[step]);
        Ok(())
    }

    fn left(&self) -> Vec<Left> {
        (self.residue.borrow().iter())
            .map(|(row, step)| Left {
                step: *step,
                what: (*row).to_owned(),
            })
            .collect()
    }

    fn pause(&self) {}
}

fn set(rows: &[&'static str]) -> BTreeSet<&'static str> {
    rows.iter().copied().collect()
}

/// Catches a row of the freshness test that no step is said to remove and that is not
/// listed as removed by none (no step left out could ever show it, and the doc comment's
/// claim would cover it by silence), a table row the freshness test does not plant (a
/// typo would make its test of necessity pass or fail for nothing), a step left out of
/// the table or out of the clean's order, and a row planted twice.
#[test]
fn the_table_covers_every_row_of_the_freshness_test_and_every_step() {
    let rows: Vec<&str> = ROWS.iter().map(|(row, _)| *row).collect();
    assert_eq!(rows.len(), set(&rows).len(), "a row is planted twice");
    let covered: BTreeSet<&str> = COVERS
        .iter()
        .flat_map(|(_, rows)| rows.iter().copied())
        .collect();
    let named: BTreeSet<&str> = covered.iter().copied().chain(UNCOVERED).collect();
    assert_eq!(
        named,
        set(&rows),
        "rows the table names differ from rows the freshness test plants"
    );
    assert!(
        covered.iter().all(|row| !UNCOVERED.contains(row)),
        "a row is both covered and listed as uncovered"
    );
    assert_eq!(
        COVERS.map(|(step, _)| step),
        STEPS,
        "the table's steps are not the clean's, in its order"
    );
    for (step, rows) in COVERS {
        assert_eq!(
            rows.is_empty(),
            UNROWED.contains(&step),
            "{step}: a step covers a row or is listed as unrowed, never both or neither"
        );
        assert_eq!(rows.len(), set(rows).len(), "{step} names a row twice");
    }
}

/// The control: catches a model whose clean leaves a row, or an option that costs the
/// whole clean something. With no step left out every step runs once, in order, and no
/// row is left.
#[test]
fn with_every_step_run_the_clean_leaves_no_row() {
    let mac = Mac::planted();
    assert!(
        mac.rows().len() >= ROWS.len() - UNCOVERED.len(),
        "the model planted {:?}",
        mac.rows()
    );
    assert_eq!(clean::clean(&mac), Outcome::Clean);
    assert_eq!(mac.rows(), BTreeSet::new());
    assert_eq!(*mac.ran.borrow(), STEPS);
}

/// What an honest check does with `step` skipped: the slot is dirty when the step
/// covers a row (what it left is found), and clean when it covers none.
fn honest_outcome(step: Step) -> &'static str {
    if covered_by(step).is_empty() {
        "clean"
    } else {
        "dirty"
    }
}

/// Leaves `step` out with a blind check and then an honest one, and reports what each
/// shows. Catches:
/// - an option that leaves out another step too, or none (the other seven run once each, in
///   order, in both runs);
/// - a blind check that is not blind or a clean that notices a missing step on its
///   own: with the check blind the clean is `Clean` and exactly the rows the table gives
///   the step are left (lease B would find them, and nothing else);
/// - an honest check that lets a missing step pass: it never ends `Clean` with a row
///   left, and a missing step is never retried or made good by a recreate: one pass,
///   dirty.
fn check_step(skip: Step) {
    let others: Vec<Step> = STEPS.iter().copied().filter(|s| *s != skip).collect();

    let blind = Mac::planted();
    let skipped = Skipped {
        skip,
        check: Check::Blind,
    };
    assert_eq!(
        clean::clean(&WithSkipped::new(&blind, skipped)),
        Outcome::Clean,
        "blind, skipping {skip}"
    );
    assert_eq!(*blind.ran.borrow(), others, "blind, skipping {skip}");
    assert_eq!(
        blind.rows(),
        covered_by(skip).iter().copied().collect(),
        "rows left with {skip} skipped and the check blind"
    );

    let honest = Mac::planted();
    let skipped = Skipped {
        check: Check::Honest,
        ..skipped
    };
    let outcome = clean::clean(&WithSkipped::new(&honest, skipped));
    let kind = match &outcome {
        Outcome::Clean => "clean",
        Outcome::Dirty { .. } => "dirty",
    };
    assert_eq!(
        kind,
        honest_outcome(skip),
        "honest, skipping {skip}: {outcome:?}"
    );
    assert_eq!(
        honest.rows().is_empty(),
        kind == "clean",
        "honest, skipping {skip}: {outcome:?}"
    );
    assert_eq!(
        honest.ran.borrow().len(),
        others.len(),
        "one pass, no retry, skipping {skip}"
    );
}

/// One test per step, named for the step, and the list of the steps they skip.
macro_rules! step_tests {
    ($($name:ident => $step:expr,)*) => {
        $(
            #[test]
            fn $name() {
                check_step($step);
            }
        )*

        const TESTED: &[Step] = &[$($step),*];
    };
}

step_tests! {
    without_the_bootout_it_leaves_its_jobs => Step::Bootout,
    without_the_schedules_sweep_it_leaves_the_crontab_and_at_job => Step::Schedules,
    without_the_kill_it_leaves_the_processes => Step::Kill,
    without_the_image_detach_it_leaves_the_image => Step::Images,
    without_the_ipc_removal_it_leaves_the_semaphore => Step::Ipc,
    without_the_flags_it_leaves_no_row_but_a_locked_home => Step::Flags,
    without_the_contents_removal_it_leaves_the_home_state_t_and_c => Step::Contents,
    without_the_shared_sweep_it_leaves_the_tmp_files => Step::Shared,
    without_the_crash_sweep_it_leaves_no_row => Step::Crashes,
    without_the_restore_it_leaves_no_row => Step::Restore,
    without_the_second_look_it_leaves_no_row => Step::Look,
}

/// Catches a step added to the clean (or one test removed) without a test of necessity:
/// the steps the tests above leave out are the clean's, each once, in its order.
#[test]
fn there_is_a_test_of_necessity_for_every_step() {
    assert_eq!(TESTED, STEPS);
}

/// Catches a test that passes vacuously (a core row unplanted), one that fails where the
/// system only refuses crontab or at (a launch daemon's context), and one that excuses a row
/// for anything but the plant's exact `refused <row>: EPERM` line.
#[test]
fn a_row_the_system_refuses_is_not_plantable_and_any_other_unplanted_row_fails() {
    let text =
        "crontab: tmp/tmp.1: Operation not permitted\nrefused crontab: EPERM\nrefused at: EPERM\n";
    let got = classify_unplanted(&["crontab", "at"], text);
    assert_eq!(got.failed, Vec::<String>::new());
    assert_eq!(
        got.not_plantable,
        vec![
            ("crontab".to_owned(), "refused crontab: EPERM".to_owned()),
            ("at".to_owned(), "refused at: EPERM".to_owned()),
        ]
    );
    // The tool's own words are not the marker; nor is another row's line, nor a row that
    // may not be excused, whatever the output says.
    let got = classify_unplanted(
        &["crontab", "at", "shared", "var-tmp", "sem"],
        "crontab: bad: Operation not permitted\nrefused shared: EPERM\nrefused var-tmp: EPERM\nrefused sem: EPERM\nrefused attic: EPERM\n",
    );
    assert_eq!(got.failed, ["crontab", "at", "shared", "var-tmp", "sem"]);
    assert!(got.not_plantable.is_empty());
    for row in CORE {
        let got = classify_unplanted(&[row], &format!("refused {row}: EPERM\n"));
        assert_eq!(
            got,
            Unplanted {
                not_plantable: Vec::new(),
                failed: vec![row.to_owned()]
            },
            "{row}"
        );
    }
    assert!(CORE.iter().all(|c| ROWS.iter().any(|(r, _)| r == c)));
}
