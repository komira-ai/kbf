//! The conformance harness, proven against a small example hook (`bin/example_hook.rs`) driven
//! on a fake service, and against copies of that hook with one planted defect each, which the
//! harness must refuse by name.

use std::ffi::OsString;

use update_hook::conformance::{
    Conformance, Fault, HookCommand, Sandbox, Violation, check, snapshot,
};
use update_hook::{Class, LineError, Outcome, Phase, Step, Verdict};

mod support;
use support::{ExampleFake, Scratch, example, example_with};

const CONVENTION: &[&str] = &["update-hook"];

fn verdict_of(
    name: &str,
    hook: &HookCommand,
    faults: &[(Step, Fault)],
) -> (Vec<(Step, Outcome)>, Verdict) {
    let scratch = Scratch::new(name);
    let mut fake = ExampleFake::default();
    let trace = Conformance::new(hook, &mut fake, scratch.path())
        .scenario(faults)
        .expect("the example hook keeps the contract")
        .expect("the example fake makes every fault");
    (trace.steps, trace.verdict)
}

// --- a correct hook passes ---

/// Catches a harness that refuses a hook that keeps the contract: the example, run both with
/// the conventional `hook_args` and with none (the step first), passes every check.
#[test]
fn a_hook_that_keeps_the_contract_passes_every_check() {
    for (name, hook) in [
        ("pass-convention", example(CONVENTION)),
        ("pass-no-hook-args", example(&[])),
        (
            "pass-irreversible",
            example(&["update-hook", "--irreversible-cutover"]),
        ),
    ] {
        let scratch = Scratch::new(name);
        let mut fake = ExampleFake::default();
        let summary =
            check(&hook, &mut fake, scratch.path()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(summary.kill_points > 0, "{name}");
        assert_eq!(summary.scenarios.len(), 16, "{name}");
        assert!(summary.skipped.is_empty(), "{name}");
        assert!(summary.scenarios.contains(&"none".to_owned()));
        assert!(
            summary
                .scenarios
                .contains(&"cutover=Fail,rollback=Fail".to_owned())
        );
    }
}

/// Catches a scenario the fake cannot make being run anyway, or hidden: it is skipped and said
/// to be.
#[test]
fn a_fault_the_fake_cannot_make_is_skipped_and_reported() {
    let scratch = Scratch::new("skip-halt");
    let mut fake = ExampleFake { no_halt: true };
    let summary = check(&example(CONVENTION), &mut fake, scratch.path()).unwrap();
    assert_eq!(summary.scenarios.len(), 12);
    assert_eq!(
        summary.skipped,
        [
            "prepare=Halt",
            "cutover=Halt",
            "verify=Halt",
            "cleanup=Halt"
        ]
    );
}

/// Catches a harness that cannot start a hook that is not there: an error, not a pass.
#[test]
fn a_hook_that_cannot_be_started_is_an_error() {
    let scratch = Scratch::new("no-such-hook");
    let hook = HookCommand::new(&scratch.path().join("absent"));
    let mut fake = ExampleFake::default();
    let err = check(&hook, &mut fake, scratch.path()).unwrap_err();
    assert!(matches!(err, Violation::Io(_)), "{err}");
}

// --- the full next-step sequences ---

/// Catches the harness driving the updater's table wrong, or the example not ending as
/// injected: success runs the four steps and reports Succeeded.
#[test]
fn the_success_sequence_runs_prepare_cutover_verify_cleanup() {
    let (steps, verdict) = verdict_of("seq-success", &example(CONVENTION), &[]);
    assert_eq!(
        steps,
        [
            (Step::Prepare, Outcome::Success),
            (Step::Cutover, Outcome::Success),
            (Step::Verify, Outcome::Success),
            (Step::Cleanup, Outcome::Success),
        ]
    );
    assert_eq!(
        verdict,
        Verdict::Succeeded {
            promotion_pending: false
        }
    );
}

/// A failure before the commit rolls back and reports Failed(before_commit); the same failure
/// of a hook whose cutover is the commit is after_commit.
#[test]
fn a_failure_of_cutover_rolls_back_before_or_after_the_commit() {
    let faults = [(Step::Cutover, Fault::Fail)];
    let (steps, verdict) = verdict_of("seq-cutover-fail", &example(CONVENTION), &faults);
    assert_eq!(
        steps,
        [
            (Step::Prepare, Outcome::Success),
            (Step::Cutover, Outcome::Failure),
            (Step::Rollback, Outcome::Success),
        ]
    );
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::BeforeCommit,
            class: Class::Release
        }
    );

    let irreversible = example(&["update-hook", "--irreversible-cutover"]);
    let (_, verdict) = verdict_of("seq-cutover-fail-irr", &irreversible, &faults);
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::AfterCommit,
            class: Class::Release
        }
    );
}

/// A failed verify rolls back at the gate when nothing irreversible happened, and after the
/// commit when cutover was the commit; it is never followed by cleanup.
#[test]
fn a_failed_verify_rolls_back_and_cleanup_never_runs() {
    let faults = [(Step::Verify, Fault::Fail)];
    let (steps, verdict) = verdict_of("seq-verify-fail", &example(CONVENTION), &faults);
    assert_eq!(steps.last(), Some(&(Step::Rollback, Outcome::Success)));
    assert!(!steps.iter().any(|(step, _)| *step == Step::Cleanup));
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release
        }
    );

    let irreversible = example(&["update-hook", "--irreversible-cutover"]);
    let (_, verdict) = verdict_of("seq-verify-fail-irr", &irreversible, &faults);
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::AfterCommit,
            class: Class::Release
        }
    );
}

/// A failed cleanup is run again forward, never rolled back, and ends as Failed(after_commit).
#[test]
fn a_failed_cleanup_is_retried_forward_and_never_rolled_back() {
    let (steps, verdict) = verdict_of(
        "seq-cleanup-fail",
        &example(CONVENTION),
        &[(Step::Cleanup, Fault::Fail)],
    );
    assert_eq!(
        steps,
        [
            (Step::Prepare, Outcome::Success),
            (Step::Cutover, Outcome::Success),
            (Step::Verify, Outcome::Success),
            (Step::Cleanup, Outcome::Failure),
            (Step::Cleanup, Outcome::Failure),
            (Step::Cleanup, Outcome::Failure),
        ]
    );
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::AfterCommit,
            class: Class::Release
        }
    );
}

/// A deferral rolls back and reports Deferred with the hook's backoff, leaving the world as it
/// was; a deferred verify is run again until its deadline, then rolled back.
#[test]
fn a_deferral_leaves_the_world_and_a_deferred_verify_is_retried_until_the_deadline() {
    let (steps, verdict) = verdict_of(
        "seq-prepare-defer",
        &example(CONVENTION),
        &[(Step::Prepare, Fault::Defer)],
    );
    assert_eq!(
        steps,
        [
            (Step::Prepare, Outcome::Deferred),
            (Step::Rollback, Outcome::Success)
        ]
    );
    assert_eq!(
        verdict,
        Verdict::Deferred {
            retry_after_secs: Some(30)
        }
    );

    let (steps, verdict) = verdict_of(
        "seq-verify-defer",
        &example(CONVENTION),
        &[(Step::Verify, Fault::Defer)],
    );
    let verifies = steps
        .iter()
        .filter(|(step, _)| *step == Step::Verify)
        .count();
    assert_eq!(verifies, 3);
    assert_eq!(steps.last(), Some(&(Step::Rollback, Outcome::Success)));
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::GateRelease,
            class: Class::Release
        }
    );
}

/// A halt stops at once: no rollback, no later step.
#[test]
fn a_halt_runs_nothing_further() {
    for (step, ran) in [
        (Step::Prepare, 1),
        (Step::Cutover, 2),
        (Step::Verify, 3),
        (Step::Cleanup, 4),
    ] {
        let (steps, verdict) = verdict_of("seq-halt", &example(CONVENTION), &[(step, Fault::Halt)]);
        assert_eq!(steps.len(), ran, "{step}");
        assert_eq!(steps.last(), Some(&(step, Outcome::Halt)));
        assert_eq!(verdict, Verdict::Halt);
    }
}

/// A rollback that fails before the commit is a halt; after the commit, a rollback that says it
/// could not give it back is Failed(after_commit).
#[test]
fn a_failing_rollback_halts_before_the_commit_and_fails_after_it() {
    let faults = [(Step::Cutover, Fault::Fail), (Step::Rollback, Fault::Fail)];
    let (steps, verdict) = verdict_of("seq-rollback-fail", &example(CONVENTION), &faults);
    assert_eq!(steps.last(), Some(&(Step::Rollback, Outcome::Failure)));
    assert_eq!(verdict, Verdict::Halt);

    let irreversible = example(&["update-hook", "--irreversible-cutover"]);
    let (_, verdict) = verdict_of("seq-rollback-fail-irr", &irreversible, &faults);
    assert_eq!(
        verdict,
        Verdict::Failed {
            phase: Phase::AfterCommit,
            class: Class::Release
        }
    );

    // A deferred rollback is run again within its bound, then treated as a failure.
    let faults = [(Step::Cutover, Fault::Fail), (Step::Rollback, Fault::Defer)];
    let (steps, verdict) = verdict_of("seq-rollback-defer", &example(CONVENTION), &faults);
    let rollbacks = steps
        .iter()
        .filter(|(step, _)| *step == Step::Rollback)
        .count();
    assert_eq!(rollbacks, 3);
    assert_eq!(verdict, Verdict::Halt);
}
