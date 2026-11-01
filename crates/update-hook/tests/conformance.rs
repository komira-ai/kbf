//! The conformance harness, proven against a small example hook (`bin/example_hook.rs`) driven
//! on a fake service, and against copies of that hook with one deliberate flaw each, which the
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

/// Catches params that differ between steps, or a staged release that is not the argument of
/// prepare: the updater writes one file per update and every step reads the same values. The
/// example hook refuses any step whose params lack them or whose prepare argument differs, so
/// the checks pass only if the harness gave every step the same file.
#[test]
fn every_step_gets_the_same_params_and_prepare_gets_the_staged_release() {
    let scratch = Scratch::new("same-params");
    let mut fake = ExampleFake::default();
    check(&example(CONVENTION), &mut fake, scratch.path()).unwrap();
    let file = scratch.path().join("run-1/control/params.toml");
    let params = update_hook::Params::read(&file).unwrap();
    assert_eq!(
        params.staged_release(),
        Some(scratch.path().join("run-1/artifact"))
    );
    assert_eq!(params.target_version(), Some("2.0.0"));
    // The artifact's identity, on every step: one file, so the same value everywhere.
    let digest = params
        .artifact_digest()
        .unwrap()
        .expect("the harness writes the digest");
    assert!(digest.starts_with("sha256:") && digest.len() == 71);
    assert_eq!(params.previous_version(), Some("1.0.0"));
    assert_eq!(
        params.previous_release(),
        Some(scratch.path().join("run-1/previous"))
    );
    assert_eq!(params.promotion_ceiling_secs(), Some(3600));
    assert_eq!(params.get_u64("promotion_bound_secs"), Some(900));
    // Written once: every sandbox's file has the same keys.
    let other =
        update_hook::Params::read(&scratch.path().join("run-2/control/params.toml")).unwrap();
    assert_eq!(other.target_version(), params.target_version());
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
// --- planted bad hooks ---

fn refused(name: &str, hook: HookCommand) -> Violation {
    let scratch = Scratch::new(name);
    let mut fake = ExampleFake::default();
    check(&hook, &mut fake, scratch.path()).expect_err("the harness must refuse this hook")
}

fn refused_scenario(name: &str, hook: HookCommand, faults: &[(Step, Fault)]) -> Violation {
    let scratch = Scratch::new(name);
    let mut fake = ExampleFake::default();
    Conformance::new(&hook, &mut fake, scratch.path())
        .scenario(faults)
        .expect_err("the harness must refuse this hook")
}

/// Catches a harness that lets a step change the world every time it runs.
#[test]
fn a_step_that_is_not_idempotent_is_refused() {
    match refused("bad-idempotent", example_with("non-idempotent")) {
        Violation::NotIdempotent { step, changes } => {
            assert_eq!(step, Step::Cutover);
            assert_eq!(changes, ["changed log"]);
        }
        other => panic!("{other}"),
    }
}

/// Catches a harness that accepts a success with no JSON last line, or one that is not JSON.
#[test]
fn a_step_that_exits_zero_without_the_line_is_refused() {
    match refused("bad-no-line", example_with("no-line-on-success")) {
        Violation::Line { step, error, .. } => {
            assert_eq!(step, Step::Verify);
            assert_eq!(error, LineError::Missing);
        }
        other => panic!("{other}"),
    }
    match refused("bad-bad-line", example_with("bad-line")) {
        Violation::Line { step, error, .. } => {
            assert_eq!(step, Step::Verify);
            assert!(matches!(error, LineError::Malformed(_)));
        }
        other => panic!("{other}"),
    }
}

/// Catches a harness that lets a hook write the service's own data directory (the old state
/// file location) or anywhere but its record.
#[test]
fn a_hook_that_writes_outside_its_record_is_refused() {
    match refused("bad-outside", example_with("writes-outside")) {
        Violation::WroteOutside { step, changes } => {
            assert_eq!(step, Step::Prepare);
            assert_eq!(changes, ["added outside/state.json"]);
        }
        other => panic!("{other}"),
    }
}

/// Catches a rollback after the commit that claims it gave everything back when it did not.
#[test]
fn a_rollback_after_the_commit_that_claims_reversible_is_refused() {
    let hook = example(&[
        "update-hook",
        "--irreversible-cutover",
        "--defect=reversible-lie",
    ]);
    match refused("bad-reversible-lie", hook) {
        Violation::RollbackClaimsReversible { changes } => {
            assert_eq!(changes, ["added stopped"]);
        }
        other => panic!("{other}"),
    }
}

/// Catches a hook that dies of a signal, exits with a code the contract does not define, or
/// defers without saying when.
#[test]
fn a_step_that_ends_outside_the_contract_is_refused() {
    assert!(matches!(
        refused("bad-signal", example_with("signal")),
        Violation::DiedBySignal { step: Step::Verify }
    ));
    assert!(matches!(
        refused("bad-exit-2", example_with("exit-2")),
        Violation::UnknownExit {
            step: Step::Verify,
            code: 2,
            ..
        }
    ));
    assert!(matches!(
        refused_scenario(
            "bad-defer",
            example_with("defer-no-retry"),
            &[(Step::Prepare, Fault::Defer)]
        ),
        Violation::DeferredWithoutRetryAfter {
            step: Step::Prepare
        }
    ));
}

/// Catches a step that ends differently the second time.
#[test]
fn a_step_whose_second_run_fails_is_refused() {
    assert!(matches!(
        refused("bad-second-run", example_with("second-run-fails")),
        Violation::RerunOutcomeChanged {
            step: Step::Cleanup,
            first: Outcome::Success,
            second: Outcome::Failure
        }
    ));
}

/// Catches a hook that cannot resume after a crash in the middle of a step.
#[test]
fn a_hook_that_diverges_after_a_crash_is_refused() {
    let scratch = Scratch::new("bad-crash");
    let hook = example_with("resume-diverges");
    let mut fake = ExampleFake::default();
    match Conformance::new(&hook, &mut fake, scratch.path()).crash_resume() {
        Err(Violation::CrashResumeDiverges {
            step,
            kill_at,
            changes,
        }) => {
            assert_eq!(step, Step::Cutover);
            assert_eq!(kill_at, 2);
            assert_eq!(changes, ["changed marker"]);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches a fake service with no kill points passing crash-resume, which would check nothing.
#[test]
fn a_hook_with_no_kill_points_is_refused() {
    let scratch = Scratch::new("bad-ungated");
    let hook = example_with("ungated");
    let mut fake = ExampleFake::default();
    assert!(matches!(
        Conformance::new(&hook, &mut fake, scratch.path()).crash_resume(),
        Err(Violation::NoKillPoints)
    ));
}

/// Catches a kill point that was in the first run and not in the next.
#[test]
fn kill_points_that_come_and_go_are_refused() {
    let scratch = Scratch::new("bad-flaky");
    let hook = example_with("flaky-gates");
    let mut fake = ExampleFake::default();
    assert!(matches!(
        Conformance::new(&hook, &mut fake, scratch.path()).crash_resume(),
        Err(Violation::KillPointVanished {
            step: Step::Prepare,
            ..
        })
    ));
}

/// Catches a cleanup that finishes an update that was rolled back, and a rollback that leaves
/// the cordon behind.
#[test]
fn a_cleanup_that_commits_after_a_rollback_and_a_rollback_that_leaves_residue_are_refused() {
    assert!(matches!(
        refused("bad-cleanup-commits", example_with("cleanup-commits")),
        Violation::CleanupCommitted { .. }
    ));
    match refused("bad-residue", example_with("leaves-cordon")) {
        Violation::ResidueLeft { step, left } => {
            assert_eq!(step, Step::Rollback);
            assert_eq!(left, ["cordon"]);
        }
        other => panic!("{other}"),
    }
}

/// Catches a hook that ignores the record lock and runs a second copy of a step.
#[test]
fn a_hook_that_runs_while_the_record_is_locked_is_refused() {
    let scratch = Scratch::new("bad-lock");
    let hook = example_with("ignores-lock");
    let mut fake = ExampleFake::default();
    assert!(matches!(
        Conformance::new(&hook, &mut fake, scratch.path()).exclusive_record(),
        Err(Violation::RanWhileLocked {
            step: Step::Prepare,
            outcome: Outcome::Success,
            ..
        })
    ));
}

/// Catches a hook that refuses while the record is locked but changes the world anyway, and
/// one whose cleanup leaves the cordon behind.
#[test]
fn a_refusal_that_changes_the_world_and_a_cleanup_that_leaves_residue_are_refused() {
    let scratch = Scratch::new("bad-lock-writes");
    let hook = example_with("writes-when-locked");
    let mut fake = ExampleFake::default();
    assert!(matches!(
        Conformance::new(&hook, &mut fake, scratch.path()).exclusive_record(),
        Err(Violation::RanWhileLocked {
            step: Step::Prepare,
            outcome: Outcome::Failure,
            ..
        })
    ));
    match refused("bad-cleanup-residue", example_with("cleanup-leaves-cordon")) {
        Violation::ResidueLeft { step, left } => {
            assert_eq!(step, Step::Cleanup);
            assert_eq!(left, ["cordon"]);
        }
        other => panic!("{other}"),
    }
}

/// Catches a hook that refuses a params file because it holds an `updater.*` key the hook does
/// not know: the updater adds reserved keys over time, and an older hook must ignore them.
#[test]
fn a_hook_that_refuses_an_unknown_updater_key_is_refused() {
    let scratch = Scratch::new("bad-unknown-key");
    let hook = example_with("refuses-unknown-updater-key");
    let mut fake = ExampleFake::default();
    assert!(matches!(
        Conformance::new(&hook, &mut fake, scratch.path()).tolerates_unknown_updater_keys(),
        Err(Violation::RefusedUpdaterKey {
            step: Step::Prepare,
            outcome: Outcome::Failure
        })
    ));
    let good = Scratch::new("good-unknown-key");
    let mut fake = ExampleFake::default();
    Conformance::new(&example(CONVENTION), &mut fake, good.path())
        .tolerates_unknown_updater_keys()
        .unwrap();
}

/// Catches a harness that writes one previous-release key and not the other: an update has the
/// previous release (a real directory in the sandbox) and version, a first install neither.
#[test]
fn the_harness_runs_an_update_and_a_first_install() {
    let scratch = Scratch::new("modes-update");
    let mut fake = ExampleFake::default();
    Conformance::new(&example(CONVENTION), &mut fake, scratch.path())
        .idempotence()
        .unwrap();
    let params =
        update_hook::Params::read(&scratch.path().join("run-1/control/params.toml")).unwrap();
    assert_eq!(
        params.previous_release(),
        Some(scratch.path().join("run-1/previous"))
    );
    assert!(scratch.path().join("run-1/previous").is_dir());
    assert_eq!(params.previous_version(), Some("1.0.0"));

    let first = Scratch::new("modes-first");
    let mut fake = ExampleFake::default();
    Conformance::new(&example(CONVENTION), &mut fake, first.path())
        .with_first_install()
        .idempotence()
        .unwrap();
    let params =
        update_hook::Params::read(&first.path().join("run-1/control/params.toml")).unwrap();
    assert_eq!(
        (params.previous_release(), params.previous_version()),
        (None, None)
    );
}

/// Catches a hook that works only with a previous release, which a first install does not have.
#[test]
fn a_hook_that_cannot_do_a_first_install_is_refused() {
    assert!(matches!(
        refused("bad-first-install", example_with("needs-previous")),
        Violation::UnexpectedOutcome {
            step: Step::Prepare,
            wanted: Outcome::Success,
            got: Outcome::Failure
        }
    ));
}

/// Catches a harness that compares the world and not the hook's record: a record that grows on
/// every rerun, or differs after a resume, is a violation too.
#[test]
fn a_record_that_grows_or_diverges_is_refused() {
    match refused("bad-record-grows", example_with("record-grows")) {
        Violation::NotIdempotent { step, changes } => {
            assert_eq!(step, Step::Cutover);
            assert_eq!(changes, ["changed record/log"]);
        }
        other => panic!("{other}"),
    }
    let scratch = Scratch::new("bad-record-resume");
    let hook = example_with("resume-diverges-record");
    let mut fake = ExampleFake::default();
    match Conformance::new(&hook, &mut fake, scratch.path()).crash_resume() {
        Err(Violation::CrashResumeDiverges { step, changes, .. }) => {
            assert_eq!(step, Step::Cutover);
            assert_eq!(changes, ["changed record/marker"]);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches crash-resume that stops at the steps of a successful update: a rollback killed
/// part-way must be resumable too.
#[test]
fn a_rollback_that_diverges_after_a_crash_is_refused() {
    let scratch = Scratch::new("bad-rollback-crash");
    let hook = example_with("rollback-diverges");
    let mut fake = ExampleFake::default();
    match Conformance::new(&hook, &mut fake, scratch.path()).crash_resume() {
        Err(Violation::CrashResumeDiverges { step, changes, .. }) => {
            assert_eq!(step, Step::Rollback);
            assert_eq!(changes, ["changed rb-marker"]);
        }
        other => panic!("{other:?}"),
    }
}

/// Catches a write guard that stops at the artifact and `outside/`: the params file and the
/// sandbox root are guarded too.
#[test]
fn a_hook_that_writes_its_params_or_the_sandbox_root_is_refused() {
    match refused("bad-writes-control", example_with("writes-control")) {
        Violation::WroteOutside { step, changes } => {
            assert_eq!(step, Step::Cutover);
            assert_eq!(changes, ["changed control/params.toml"]);
        }
        other => panic!("{other}"),
    }
    match refused("bad-writes-root", example_with("writes-root")) {
        Violation::WroteOutside { step, changes } => {
            assert_eq!(step, Step::Cutover);
            assert_eq!(changes, ["added stray"]);
        }
        other => panic!("{other}"),
    }
}

/// Catches a step that changes things with no kill point being hidden by other steps that have
/// them.
#[test]
fn a_step_with_no_kill_point_is_refused_even_when_others_have_them() {
    let scratch = Scratch::new("bad-ungated-cutover");
    let hook = example_with("ungated-cutover");
    let mut fake = ExampleFake::default();
    assert!(matches!(
        Conformance::new(&hook, &mut fake, scratch.path()).crash_resume(),
        Err(Violation::StepWithoutKillPoints {
            step: Step::Cutover
        })
    ));
}

/// Catches a step whose `reversible` differs between two runs.
#[test]
fn a_step_whose_reversible_flips_on_the_second_run_is_refused() {
    assert!(matches!(
        refused("bad-reversible-flips", example_with("reversible-flips")),
        Violation::RerunReversibleChanged {
            step: Step::Cleanup,
            first: false,
            second: true
        }
    ));
}

/// Catches a fake whose injected fault the hook does not honour, and a deferral that leaves the
/// world changed.
#[test]
fn a_hook_that_ignores_a_fault_or_keeps_state_after_a_deferral_is_refused() {
    assert!(matches!(
        refused_scenario(
            "bad-ignores-faults",
            example_with("ignores-faults"),
            &[(Step::Cutover, Fault::Fail)]
        ),
        Violation::UnexpectedOutcome {
            step: Step::Cutover,
            wanted: Outcome::Failure,
            got: Outcome::Success
        }
    ));
    assert!(matches!(
        refused_scenario(
            "bad-defer-state",
            example_with("rollback-says-irreversible"),
            &[(Step::Prepare, Fault::Defer)]
        ),
        Violation::DeferredLeftState { .. }
    ));
}

/// Catches a violation that prints nothing useful.
#[test]
fn every_violation_says_what_it_found() {
    let steps = Step::Verify;
    let all = vec![
        Violation::Io(std::io::Error::other("x")),
        Violation::DiedBySignal { step: steps },
        Violation::UnknownExit {
            step: steps,
            code: 2,
            stderr: "s".into(),
        },
        Violation::Line {
            step: steps,
            error: LineError::Missing,
            stderr: "s".into(),
        },
        Violation::DeferredWithoutRetryAfter { step: steps },
        Violation::WroteOutside {
            step: steps,
            changes: vec!["c".into()],
        },
        Violation::UnexpectedOutcome {
            step: steps,
            wanted: Outcome::Success,
            got: Outcome::Halt,
        },
        Violation::RerunOutcomeChanged {
            step: steps,
            first: Outcome::Success,
            second: Outcome::Halt,
        },
        Violation::NotIdempotent {
            step: steps,
            changes: vec!["c".into()],
        },
        Violation::RollbackClaimsReversible {
            changes: vec!["c".into()],
        },
        Violation::ResidueLeft {
            step: steps,
            left: vec!["l".into()],
        },
        Violation::CleanupCommitted {
            changes: vec!["c".into()],
        },
        Violation::NoKillPoints,
        Violation::KillPointVanished {
            step: steps,
            kill_at: 3,
        },
        Violation::CrashResumeDiverges {
            step: steps,
            kill_at: 3,
            changes: vec!["c".into()],
        },
        Violation::DeferredLeftState {
            changes: vec!["c".into()],
        },
        Violation::RanWhileLocked {
            step: steps,
            outcome: Outcome::Success,
            changes: vec![],
        },
    ];
    for violation in all {
        assert!(!violation.to_string().is_empty());
    }
}

/// Catches the hook command's defaults: `update-hook` before the step, replaceable.
#[test]
fn hook_args_default_to_the_convention() {
    let hook = HookCommand::new(std::path::Path::new("/x/hook"));
    assert_eq!(hook.hook_args, [OsString::from("update-hook")]);
    assert!(hook.with_hook_args(&[]).hook_args.is_empty());
}

/// Catches a sandbox whose world is not the one the fake set up.
#[test]
fn the_example_world_starts_at_v1() {
    let scratch = Scratch::new("world-start");
    let sandbox = Sandbox::create(scratch.path().join("run")).unwrap();
    let mut fake = ExampleFake::default();
    update_hook::conformance::FakeService::init(&mut fake, &sandbox).unwrap();
    assert!(
        snapshot(&sandbox.world())
            .unwrap()
            .contains_key(std::path::Path::new("current"))
    );
}
