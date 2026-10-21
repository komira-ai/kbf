//! The mutant harness over the steps of the slot clean, on a model Mac, on every
//! platform. `tests/slot_clean.rs` has the real run (`#[ignore]`, root, macOS).
//!
//! The freshness test of `tests/slot_clean.rs` claims that skipping any step of the clean
//! turns that step's rows red. Nothing ran that claim. The seam is
//! [`Mutant`]/[`Mutated`] (`src/slots/clean.rs`): one step skipped, the check honest
//! (design: mutant A) or blind (mutant B). Here the clean runs through it over a model
//! Mac whose residue is the rows of [`support::slot_rows::COVERS`], each removed by its
//! step, and the recreate moves the home and the `/var/folders` entry aside.
//!
//! What this proves, and what it does not:
//! - It proves the harness: the seam skips exactly the step it names; a blind check lets
//!   the residue through as a clean (so it would reach lease B), exactly the rows of
//!   that step; an honest check never reports a clean while a row is left (the slot is
//!   quarantined, or recreated when the recreate removes the rows, and a survivor is
//!   never recreated). It proves the table is whole: every row of the freshness test is
//!   under a step or listed as removed by none.
//! - It does not prove the table. The model removes a row because the table says its
//!   step does, so these tests cannot catch a table that is wrong about the real steps.
//!   That is `tests/slot_clean.rs`' mutant tests, which run the same table against the
//!   real steps on a Mac, and they have not run yet.

mod support;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use kbf_driver_native::slots::clean::{
    self, Left, Mutant, Mutated, Os, Outcome, STEPS, Step, Verify,
};
use support::slot_rows::{COVERS, ROWS, UNCOVERED, covered_by};

/// A row left by the lease, and the step that removes it.
type Residue = (&'static str, Step);

/// A Mac with every row of [`COVERS`] planted. Each step removes the residue the table
/// gives it; the look names what is left, under the step that should have removed it;
/// the recreate moves the home and the `/var/folders` entry aside, so it removes what
/// [`Step::Home`] and [`Step::Folders`] remove, and nothing else (a loaded job, a
/// schedule, an image, an IPC object or a process of the uid survives a recreate).
struct Mac {
    residue: RefCell<BTreeSet<Residue>>,
    ran: RefCell<Vec<Step>>,
    recreated: Cell<usize>,
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
            recreated: Cell::new(0),
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

    fn recreate(&self) -> Result<(), String> {
        self.recreated.set(self.recreated.get() + 1);
        self.removing(&[Step::Home, Step::Folders]);
        Ok(())
    }
}

fn set(rows: &[&'static str]) -> BTreeSet<&'static str> {
    rows.iter().copied().collect()
}

/// Catches a row of the freshness test that no step is said to remove and that is not
/// listed as removed by none (a mutant could never turn it red, and the doc comment's
/// claim would cover it by silence), a table row the freshness test does not plant (a
/// typo would make its mutant test pass or fail for nothing), a step left out of the
/// table or out of the clean's order, and a row planted twice.
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
        assert!(!rows.is_empty(), "{step} covers no row");
        assert_eq!(rows.len(), set(rows).len(), "{step} names a row twice");
    }
}

/// The control: catches a model whose clean leaves a row, or a seam that costs the
/// unmutated clean something. Without a mutant every step runs once, in order, and no
/// row is left.
#[test]
fn without_a_mutant_the_clean_leaves_no_row() {
    let mac = Mac::planted();
    assert!(
        mac.rows().len() >= ROWS.len() - UNCOVERED.len(),
        "the model planted {:?}",
        mac.rows()
    );
    assert_eq!(clean::clean(&mac), Outcome::Clean);
    assert_eq!(mac.rows(), BTreeSet::new());
    assert_eq!(*mac.ran.borrow(), STEPS);
    assert_eq!(mac.recreated.get(), 0);
}

/// What an honest check does with `step` skipped: the survivors' step is never
/// recreated; the home and the `/var/folders` entry are removed by the recreate
/// (the slot is back, recreated); everything else survives it, so the slot is
/// quarantined.
fn honest_outcome(step: Step) -> &'static str {
    match step {
        Step::Home | Step::Folders => "recreated",
        Step::Bootout | Step::Schedules | Step::Kill | Step::Images | Step::Ipc | Step::Shared => {
            "quarantined"
        }
    }
}

/// Skips `step` with a blind check and then an honest one, and reports what each
/// shows. Catches:
/// - a seam that skips another step too, or none (the other seven run once each, in
///   order, in both runs);
/// - a blind check that is not blind or a clean that notices a skipped step on its
///   own: with the check blind the clean is `Clean` and exactly the rows the table gives
///   the step are left (lease B would find them, and nothing else);
/// - an honest check that lets a skipped step pass: it never ends `Clean` with a row
///   left, ends recreated only when the recreate removed every row, and a skipped
///   [`Step::Kill`] is never recreated (the processes it leaves would run on as an
///   orphan uid).
fn check_step(skip: Step) {
    let others: Vec<Step> = STEPS.iter().copied().filter(|s| *s != skip).collect();

    let blind = Mac::planted();
    let mutant = Mutant {
        skip,
        verify: Verify::Blind,
    };
    assert_eq!(
        clean::clean(&Mutated::new(&blind, mutant)),
        Outcome::Clean,
        "blind, skipping {skip}"
    );
    assert_eq!(*blind.ran.borrow(), others, "blind, skipping {skip}");
    assert_eq!(
        blind.rows(),
        covered_by(skip).iter().copied().collect(),
        "rows left with {skip} skipped and the check blind"
    );
    assert_eq!(blind.recreated.get(), 0);

    let honest = Mac::planted();
    let mutant = Mutant {
        verify: Verify::Honest,
        ..mutant
    };
    let outcome = clean::clean(&Mutated::new(&honest, mutant));
    let kind = match &outcome {
        Outcome::Clean => "clean",
        Outcome::Recreated { .. } => "recreated",
        Outcome::Quarantined { .. } => "quarantined",
    };
    assert_eq!(
        kind,
        honest_outcome(skip),
        "honest, skipping {skip}: {outcome:?}"
    );
    if kind == "recreated" {
        assert_eq!(honest.rows(), BTreeSet::new(), "recreated with rows left");
    } else {
        assert!(!honest.rows().is_empty(), "quarantined with nothing left");
    }
    let recreates = usize::from(skip != Step::Kill);
    assert_eq!(
        honest.recreated.get(),
        recreates,
        "honest, skipping {skip}: {outcome:?}"
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
    skipping_the_bootout_leaves_its_jobs => Step::Bootout,
    skipping_the_schedules_sweep_leaves_the_crontab_and_at_job => Step::Schedules,
    skipping_the_kill_leaves_the_processes => Step::Kill,
    skipping_the_image_detach_leaves_the_image => Step::Images,
    skipping_the_ipc_removal_leaves_the_semaphore => Step::Ipc,
    skipping_the_home_wipe_leaves_the_home_state => Step::Home,
    skipping_the_folders_wipe_leaves_t_and_c => Step::Folders,
    skipping_the_shared_sweep_leaves_the_tmp_files => Step::Shared,
}

/// Catches a step added to the clean (or one test removed) without a mutant test: the
/// steps the tests above skip are the clean's, each once, in its order.
#[test]
fn there_is_a_mutant_test_for_every_step() {
    assert_eq!(TESTED, STEPS);
}
