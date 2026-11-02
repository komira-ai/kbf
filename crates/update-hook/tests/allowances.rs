//! The harness's allowances (`conformance::Allowances`): three behaviours a hook may show for
//! safety, each refused by the strict harness, accepted when the hook's test declares it, and
//! accepted no further than the allowance says. The hook is the example hook
//! (`bin/example_hook.rs`) with a `--behaviour=` switch for each.

use std::path::PathBuf;

use update_hook::conformance::{Allowances, Conformance, HookCommand, Summary, Violation};
use update_hook::{Outcome, Step};

mod support;
use support::{ExampleFake, Scratch, example};

/// The example hook with these behaviours, and its commit at `cutover` when `irreversible`.
fn behaving(irreversible: bool, behaviours: &[&str]) -> HookCommand {
    let mut args = vec!["update-hook".to_owned()];
    if irreversible {
        args.push("--irreversible-cutover".to_owned());
    }
    args.extend(behaviours.iter().map(|b| format!("--behaviour={b}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    example(&args)
}

/// Every check of the harness on `hook`, accepting what `allow` declares.
fn check_all(name: &str, hook: &HookCommand, allow: Allowances) -> Result<Summary, Violation> {
    let scratch = Scratch::new(name);
    let mut fake = ExampleFake::default();
    Conformance::new(hook, &mut fake, scratch.path())
        .with_allowances(allow)
        .check_all()
}

fn rerun_reversible() -> Allowances {
    Allowances {
        rollback_rerun_may_say_reversible: true,
        ..Allowances::default()
    }
}

fn lapsing(path: &str) -> Allowances {
    Allowances {
        lapsing_cordon: vec![PathBuf::from(path)],
        ..Allowances::default()
    }
}

fn first_install_rollback() -> Allowances {
    Allowances {
        first_install_rollback_may_fail: true,
        ..Allowances::default()
    }
}

/// Every allowance at once: what a hook that shows all three behaviours declares.
fn all_three() -> Allowances {
    Allowances {
        rollback_rerun_may_say_reversible: true,
        lapsing_cordon: vec![PathBuf::from("cordon")],
        first_install_rollback_may_fail: true,
    }
}

// --- a hook that declares nothing is checked strictly ---

/// Catches an allowance that is on by default: the harness without allowances is the strict
/// one, and a hook that keeps the contract passes it with or without all three declared.
#[test]
fn the_default_declares_nothing_and_a_strict_hook_passes_either_way() {
    assert_eq!(
        Allowances::default(),
        Allowances {
            rollback_rerun_may_say_reversible: false,
            lapsing_cordon: Vec::new(),
            first_install_rollback_may_fail: false,
        }
    );
    for (name, hook) in [
        ("allow-strict", example(&["update-hook"])),
        (
            "allow-strict-irr",
            example(&["update-hook", "--irreversible-cutover"]),
        ),
    ] {
        check_all(name, &hook, Allowances::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
        check_all(name, &hook, all_three()).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

// --- C1: a rollback rerun that finds no record may say reversible:true ---

/// Catches a harness that refuses a rollback rerun saying `reversible: true` after a first run
/// that said `false` when the hook declared it, or accepts it when it did not.
#[test]
fn a_rollback_rerun_that_says_reversible_passes_only_when_declared() {
    let hook = behaving(true, &["forgets-commit"]);
    match check_all("c1-strict", &hook, Allowances::default()) {
        Err(Violation::RerunReversibleChanged {
            step: Step::Rollback,
            first: false,
            second: true,
        }) => {}
        other => panic!("{other:?}"),
    }
    check_all("c1-declared", &hook, rerun_reversible()).expect("declared: accepted");
}

/// Catches an allowance for the flag that also lets the outcome change: a rollback rerun that
/// fails where the first succeeded is refused, declared or not.
#[test]
fn a_rollback_rerun_with_another_outcome_is_refused_even_when_declared() {
    let hook = behaving(true, &["forgets-commit", "rollback-rerun-fails"]);
    match check_all("c1-outcome", &hook, rerun_reversible()) {
        Err(Violation::RerunOutcomeChanged {
            step: Step::Rollback,
            first: Outcome::Success,
            second: Outcome::Failure,
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// Catches an allowance that reaches past `rollback`: a cleanup whose `reversible` changes on
/// the second run is refused with it declared.
#[test]
fn the_rollback_allowance_does_not_cover_another_step() {
    let hook = example(&["update-hook", "--defect=reversible-flips"]);
    match check_all("c1-cleanup", &hook, rerun_reversible()) {
        Err(Violation::RerunReversibleChanged {
            step: Step::Cleanup,
            first: false,
            second: true,
        }) => {}
        other => panic!("{other:?}"),
    }
}

// --- C2: a cordon left to lapse after a crash between placing and recording it ---

/// Catches a harness that refuses a cordon left after a crash right after it was placed and
/// before the hook recorded it, when the hook declared it, or accepts it when it did not. The
/// crash is the third kill point of `prepare`: after `next` and the cordon.
#[test]
fn a_cordon_left_by_a_crash_before_its_record_passes_only_when_declared() {
    let hook = behaving(false, &["cordon-unowned"]);
    match check_all("c2-strict", &hook, Allowances::default()) {
        Err(Violation::CrashResumeDiverges {
            step: Step::Prepare,
            kill_at: 3,
            changes,
        }) => assert_eq!(changes, ["added cordon"]),
        other => panic!("{other:?}"),
    }
    check_all("c2-declared", &hook, lapsing("cordon")).expect("declared: accepted");
}

/// Catches a window wider than the one kill point right after the cordon was placed: a hook
/// that records its cordon two changes later leaves it after a crash at the fourth point, and
/// that is refused although the third (the window) is accepted.
#[test]
fn a_cordon_left_by_a_crash_outside_the_window_is_refused_even_when_declared() {
    let hook = behaving(false, &["cordon-recorded-late"]);
    match check_all("c2-late", &hook, lapsing("cordon")) {
        Err(Violation::CrashResumeDiverges {
            step: Step::Prepare,
            kill_at: 4,
            changes,
        }) => assert_eq!(changes, ["added cordon"]),
        other => panic!("{other:?}"),
    }
}

/// Catches an allowance that sets aside more than the declared cordon: in the window, another
/// path left behind is refused, and so is the cordon itself when the declaration names another
/// path.
#[test]
fn in_the_window_only_the_declared_cordon_is_set_aside() {
    let hook = behaving(false, &["cordon-unowned-noted"]);
    match check_all("c2-noted", &hook, lapsing("cordon")) {
        Err(Violation::CrashResumeDiverges {
            step: Step::Prepare,
            kill_at: 3,
            changes,
        }) => assert_eq!(changes, ["added foreign-cordon"]),
        other => panic!("{other:?}"),
    }
    let hook = behaving(false, &["cordon-unowned"]);
    match check_all("c2-other-path", &hook, lapsing("drain")) {
        Err(Violation::CrashResumeDiverges {
            step: Step::Prepare,
            kill_at: 3,
            changes,
        }) => assert_eq!(changes, ["added cordon"]),
        other => panic!("{other:?}"),
    }
}

// --- C3: a first-install rollback after cutover may fail with no_previous_release ---

/// Catches a harness that refuses a first-install rollback failing with `no_previous_release`
/// when the hook declared it, or accepts it when it did not. The update half of the checks
/// still requires the rollback to succeed, and it does.
#[test]
fn a_first_install_rollback_that_fails_for_no_previous_release_passes_only_when_declared() {
    let hook = behaving(false, &["first-install-no-rollback"]);
    match check_all("c3-strict", &hook, Allowances::default()) {
        Err(Violation::UnexpectedOutcome {
            step: Step::Rollback,
            wanted: Outcome::Success,
            got: Outcome::Failure,
        }) => {}
        other => panic!("{other:?}"),
    }
    check_all("c3-declared", &hook, first_install_rollback()).expect("declared: accepted");
}

/// Catches an allowance that reaches an update: a rollback that fails with
/// `no_previous_release` while the params name a previous release is refused, declared or not.
#[test]
fn a_rollback_failure_with_a_previous_release_present_is_refused_even_when_declared() {
    let scratch = Scratch::new("c3-update");
    let hook = behaving(false, &["no-rollback-after-cutover"]);
    let mut fake = ExampleFake::default();
    match Conformance::new(&hook, &mut fake, scratch.path())
        .with_allowances(first_install_rollback())
        .idempotence()
    {
        Err(Violation::UnexpectedOutcome {
            step: Step::Rollback,
            wanted: Outcome::Success,
            got: Outcome::Failure,
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// Catches an allowance for any failure of a first-install rollback: one with a reason other
/// than `no_previous_release` is refused, declared or not.
#[test]
fn a_first_install_rollback_failure_for_another_reason_is_refused_even_when_declared() {
    let scratch = Scratch::new("c3-reason");
    let hook = behaving(false, &["first-install-no-rollback-busy"]);
    let mut fake = ExampleFake::default();
    match Conformance::new(&hook, &mut fake, scratch.path())
        .with_first_install()
        .with_allowances(first_install_rollback())
        .idempotence()
    {
        Err(Violation::UnexpectedOutcome {
            step: Step::Rollback,
            wanted: Outcome::Success,
            got: Outcome::Failure,
        }) => {}
        other => panic!("{other:?}"),
    }
}

/// Catches a declined first-install rollback held to less than "fails the same way and changes
/// nothing": one that took something away before it declined, one whose rerun fails with
/// another reason, one whose rerun rolls back, and one whose rerun takes something away, are
/// refused with the allowance declared.
#[test]
fn a_declined_first_install_rollback_that_changes_things_or_its_reason_is_refused() {
    let declined = |name: &str, behaviour: &str| {
        let scratch = Scratch::new(name);
        let hook = behaving(false, &[behaviour]);
        let mut fake = ExampleFake::default();
        Conformance::new(&hook, &mut fake, scratch.path())
            .with_first_install()
            .with_allowances(first_install_rollback())
            .idempotence()
    };
    match declined("c3-undoes", "first-install-no-rollback-undoes") {
        Err(Violation::DeclinedRollbackChanged { changes }) => {
            assert_eq!(changes, ["removed next"]);
        }
        other => panic!("{other:?}"),
    }
    match declined("c3-then-busy", "first-install-no-rollback-then-busy") {
        Err(Violation::RerunReasonChanged {
            step: Step::Rollback,
            first,
            second,
        }) => {
            assert_eq!(first.as_deref(), Some("no_previous_release"));
            assert_eq!(second.as_deref(), Some("agent_busy"));
        }
        other => panic!("{other:?}"),
    }
    match declined("c3-once", "first-install-no-rollback-once") {
        Err(Violation::RerunOutcomeChanged {
            step: Step::Rollback,
            first: Outcome::Failure,
            second: Outcome::Success,
        }) => {}
        other => panic!("{other:?}"),
    }
    match declined("c3-rerun-undoes", "first-install-no-rollback-rerun-undoes") {
        Err(Violation::NotIdempotent {
            step: Step::Rollback,
            changes,
        }) => assert_eq!(changes, ["removed next"]),
        other => panic!("{other:?}"),
    }
}

/// Catches a harness that, accepting a declined first-install rollback, still judges it as one
/// that ran: the update stays cut over with its record kept, and the check passes.
#[test]
fn a_declined_first_install_rollback_leaves_the_update_cut_over() {
    let scratch = Scratch::new("c3-twice");
    let hook = behaving(false, &["first-install-no-rollback"]);
    let mut fake = ExampleFake::default();
    Conformance::new(&hook, &mut fake, scratch.path())
        .with_first_install()
        .with_allowances(first_install_rollback())
        .idempotence()
        .expect("declared: accepted");
    // The second sandbox is the rollback's: the update stayed cut over, the record kept.
    let rolled = scratch.path().join("run-2");
    assert_eq!(
        std::fs::read_link(rolled.join("world/current")).unwrap(),
        PathBuf::from("v2")
    );
    assert!(rolled.join("record/cutover").exists());
}

/// Catches the three allowances interfering: a hook that shows all three behaviours passes
/// every check with all three declared.
#[test]
fn a_hook_with_all_three_behaviours_passes_with_all_three_declared() {
    let hook = behaving(
        true,
        &[
            "forgets-commit",
            "cordon-unowned",
            "first-install-no-rollback",
        ],
    );
    let summary = check_all("all-three", &hook, all_three()).expect("declared: accepted");
    assert!(summary.kill_points > 0);
    assert!(summary.skipped.is_empty());
}
