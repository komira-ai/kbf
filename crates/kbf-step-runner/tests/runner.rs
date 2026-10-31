//! The step runner: what runs after each outcome, what is persisted, and what never runs.

mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_step_runner::record::{RECORD_FILE, State};
use kbf_step_runner::runner::{DEFAULT_RETRY_SECS, ROLLBACK_ATTEMPTS, WATCH_POLL_SECS};
use kbf_step_runner::{Finished, Loaded, RunError, Staged};
use support::{DEFER, ENV_FAIL, FAIL, HALT, IRREVERSIBLE_OK, OK, PENDING, Reply, Rig};
use update_hook::{Class, Params, Phase, Step, Verdict};

const ROLLBACK_STUCK: Reply = Reply::Exit(1, Some(r#"{"reversible":false}"#));

fn fin(verdict: Verdict) -> Finished {
    Finished {
        verdict,
        reason: None,
    }
}

fn failed(phase: Phase, class: Class) -> Finished {
    fin(Verdict::Failed { phase, class })
}

fn ok() -> Finished {
    fin(Verdict::Succeeded {
        promotion_pending: false,
    })
}

fn run(rig: &Rig) -> Finished {
    rig.runner().run(&rig.order()).unwrap()
}

/// Catches a runner that skips a step, runs them out of order, or loses what a step needs: the
/// five-step success path runs `prepare`, `cutover`, `verify`, `cleanup` once each from the
/// new release, each in the unit named for it, with its own bound, and `prepare` alone is given
/// the artifact.
#[test]
fn a_successful_update_runs_the_four_steps_from_the_new_release() {
    let rig = Rig::new("run-ok");
    assert_eq!(run(&rig), ok());
    assert_eq!(
        rig.fake.steps(),
        [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup]
    );
    let runs = rig.fake.runs();
    let fields: Vec<_> = runs
        .iter()
        .map(|e| match e {
            support::Event::Run {
                unit,
                executable,
                timeout_secs,
                artifact,
                ..
            } => (
                unit.clone(),
                executable.clone(),
                *timeout_secs,
                artifact.clone(),
            ),
            support::Event::Stop(_) => unreachable!(),
        })
        .collect();
    let hook = rig.new_dir.join("hook");
    assert_eq!(
        fields,
        [
            (
                "updater-svc-prepare-1".to_owned(),
                hook.clone(),
                600,
                Some(rig.new_dir.clone())
            ),
            ("updater-svc-cutover-1".to_owned(), hook.clone(), 300, None),
            ("updater-svc-verify-1".to_owned(), hook.clone(), 1000, None),
            ("updater-svc-cleanup-1".to_owned(), hook, 1300, None),
        ]
    );
    assert!(rig.fake.stops().is_empty());
}

/// Catches a params file that differs between steps, loses a reserved key, or lets the
/// artifact of `prepare` differ from `updater.staged_release`: one file, the same bytes in
/// every step, the declared table beside the five reserved keys.
#[test]
fn every_step_gets_the_same_params_file() {
    let rig = Rig::new("run-params");
    run(&rig);
    let runs = rig.fake.runs();
    let texts: Vec<_> = runs
        .iter()
        .map(|e| match e {
            support::Event::Run {
                params,
                params_file,
                ..
            } => {
                assert_eq!(params_file, &rig.state.join("params.toml"));
                params.clone()
            }
            support::Event::Stop(_) => unreachable!(),
        })
        .collect();
    assert_eq!(texts.len(), 4);
    assert!(texts.iter().all(|t| t == &texts[0]));
    let params = Params::parse(&texts[0]).unwrap();
    assert_eq!(params.staged_release(), Some(rig.new_dir.clone()));
    assert_eq!(params.previous_release(), Some(rig.old_dir.clone()));
    assert_eq!(params.target_version(), Some("2.0.0"));
    assert_eq!(params.previous_version(), Some("1.0.0"));
    assert_eq!(params.promotion_ceiling_secs(), Some(3600));
    assert_eq!(params.get_u64("promotion_bound_secs"), Some(900));
    let support::Event::Run { artifact, .. } = &runs[0] else {
        unreachable!()
    };
    assert_eq!(artifact.as_deref(), params.staged_release().as_deref());
}

/// Catches reserved keys written for a first install that has no previous release.
#[test]
fn a_first_install_has_no_previous_keys() {
    let rig = Rig::new("run-first");
    let mut order = rig.order();
    order.previous = None;
    order.previous_version = None;
    rig.runner().run(&order).unwrap();
    let support::Event::Run { params, .. } = &rig.fake.runs()[0] else {
        unreachable!()
    };
    let params = Params::parse(params).unwrap();
    assert_eq!(params.previous_release(), None);
    assert_eq!(params.previous_version(), None);
    assert_eq!(params.target_version(), Some("2.0.0"));
}

/// Catches a failed or deferred `prepare` or a killed `cutover` that does not roll back, or that
/// runs `cleanup`: after each the hook's own `rollback` runs and the original verdict is reported.
#[test]
fn a_failure_before_the_commit_rolls_back_and_reports_the_failure() {
    let rig = Rig::new("run-prepare-fails");
    rig.fake.say(Step::Prepare, &[FAIL]);
    assert_eq!(run(&rig), failed(Phase::BeforeCommit, Class::Release));
    assert_eq!(rig.fake.steps(), [Step::Prepare, Step::Rollback]);

    let rig = Rig::new("run-prepare-defers");
    rig.fake.say(Step::Prepare, &[DEFER]);
    assert_eq!(
        run(&rig),
        fin(Verdict::Deferred {
            retry_after_secs: Some(100)
        })
    );
    assert_eq!(rig.fake.steps(), [Step::Prepare, Step::Rollback]);

    let rig = Rig::new("run-cutover-killed");
    rig.fake.say(Step::Cutover, &[Reply::Killed]);
    assert_eq!(run(&rig), failed(Phase::BeforeCommit, Class::Release));
    assert_eq!(
        rig.fake.steps(),
        [Step::Prepare, Step::Cutover, Step::Rollback]
    );

    let rig = Rig::new("run-cutover-defers");
    rig.fake.say(Step::Cutover, &[DEFER]);
    assert_eq!(
        run(&rig),
        fin(Verdict::Deferred {
            retry_after_secs: Some(100)
        })
    );
}

/// Catches a failed `verify` that runs `cleanup`, or a class lost on the way: the hook's class
/// and reason reach the verdict, and the phase says whether the commit had happened.
#[test]
fn a_failed_verify_rolls_back_with_the_hooks_class_and_reason() {
    let rig = Rig::new("run-verify-env");
    rig.fake.say(Step::Verify, &[ENV_FAIL]);
    assert_eq!(
        run(&rig),
        Finished {
            verdict: Verdict::Failed {
                phase: Phase::GateEnvironment,
                class: Class::Environment
            },
            reason: Some("remote_down".to_owned())
        }
    );
    assert_eq!(
        rig.fake.steps(),
        [Step::Prepare, Step::Cutover, Step::Verify, Step::Rollback]
    );

    let rig = Rig::new("run-verify-release");
    rig.fake.say(Step::Verify, &[FAIL]);
    assert_eq!(run(&rig), failed(Phase::GateRelease, Class::Release));

    let rig = Rig::new("run-verify-after-commit");
    rig.fake.say(Step::Cutover, &[IRREVERSIBLE_OK]);
    rig.fake.say(Step::Verify, &[FAIL]);
    assert_eq!(run(&rig), failed(Phase::AfterCommit, Class::Release));
    assert!(!rig.fake.steps().contains(&Step::Cleanup));
}

/// Catches a `verify` that is deferred and not tried again, or tried again without the wait the
/// hook asked for: it is run again after `retry_after_secs`, the unit name stays, and success
/// goes on to `cleanup`.
#[test]
fn a_deferred_verify_is_run_again_after_its_wait() {
    let rig = Rig::new("run-verify-again");
    rig.fake.say(Step::Verify, &[DEFER, DEFER, OK]);
    assert_eq!(run(&rig), ok());
    assert_eq!(
        rig.fake.steps(),
        [
            Step::Prepare,
            Step::Cutover,
            Step::Verify,
            Step::Verify,
            Step::Verify,
            Step::Cleanup
        ]
    );
    assert_eq!(rig.clock.slept(), [100, 100]);
}

/// Catches a verify window that is not enforced: a `verify` that keeps deferring past
/// `verify_deadline_secs` is followed by `rollback`, and each run is bounded by the time the
/// window has left.
#[test]
fn verify_that_defers_past_the_window_rolls_back() {
    let rig = Rig::new("run-window");
    let mut order = rig.order();
    order.bounds.verify_deadline_secs = 250;
    rig.fake.say(Step::Verify, &[DEFER, DEFER, DEFER, DEFER]);
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(got, failed(Phase::GateRelease, Class::Release));
    let timeouts: Vec<u64> = rig
        .fake
        .runs()
        .iter()
        .filter_map(|e| match e {
            support::Event::Run {
                step: Step::Verify,
                timeout_secs,
                ..
            } => Some(*timeout_secs),
            _ => None,
        })
        .collect();
    assert_eq!(timeouts, [250, 150, 50]);
    assert_eq!(rig.fake.steps().last(), Some(&Step::Rollback));
    assert_eq!(rig.clock.slept(), [100, 100]);
}

/// Catches a rollback that fails being read as a failure to page for when nothing can repair the
/// host: before the commit it is a `halt`; after the commit it is `Failed(after_commit)` only
/// when the hook says it is irreversible, and a halt when it says nothing.
#[test]
fn a_failed_rollback_is_a_halt_before_the_commit() {
    let rig = Rig::new("run-rollback-fails");
    rig.fake.say(Step::Prepare, &[FAIL]);
    rig.fake.say(Step::Rollback, &[FAIL]);
    assert_eq!(run(&rig), fin(Verdict::Halt));

    let rig = Rig::new("run-rollback-fails-after-commit");
    rig.fake.say(Step::Cutover, &[IRREVERSIBLE_OK]);
    rig.fake.say(Step::Verify, &[FAIL]);
    rig.fake.say(Step::Rollback, &[ROLLBACK_STUCK]);
    assert_eq!(run(&rig), failed(Phase::AfterCommit, Class::Release));

    let rig = Rig::new("run-rollback-silent-after-commit");
    rig.fake.say(Step::Cutover, &[IRREVERSIBLE_OK]);
    rig.fake.say(Step::Verify, &[FAIL]);
    rig.fake.say(Step::Rollback, &[FAIL]);
    assert_eq!(run(&rig), fin(Verdict::Halt));
}

/// Catches a halt that runs another step: `76` from any step ends the update with nothing more
/// run, `rollback` and `cleanup` included.
#[test]
fn a_halt_runs_nothing_further() {
    for (step, ran) in [
        (Step::Prepare, 1),
        (Step::Cutover, 2),
        (Step::Verify, 3),
        (Step::Cleanup, 4),
    ] {
        let rig = Rig::new(&format!("run-halt-{step}"));
        rig.fake.say(step, &[HALT]);
        assert_eq!(run(&rig), fin(Verdict::Halt));
        assert_eq!(rig.fake.steps().len(), ran, "{step}");
    }
}

/// Catches a deferred `rollback` that is never repeated, or repeated for ever: it is run again
/// `ROLLBACK_ATTEMPTS` times and then read as a failure.
#[test]
fn a_deferred_rollback_is_run_again_a_bounded_number_of_times() {
    let rig = Rig::new("run-rollback-defers");
    rig.fake.say(Step::Prepare, &[FAIL]);
    rig.fake.say(Step::Rollback, &[DEFER, DEFER, OK]);
    assert_eq!(run(&rig), failed(Phase::BeforeCommit, Class::Release));
    assert_eq!(rig.fake.steps().len(), 4);

    let rig = Rig::new("run-rollback-defers-for-ever");
    rig.fake.say(Step::Prepare, &[FAIL]);
    rig.fake.say(Step::Rollback, &[DEFER; 10]);
    assert_eq!(run(&rig), fin(Verdict::Halt));
    let rollbacks = rig
        .fake
        .steps()
        .into_iter()
        .filter(|s| *s == Step::Rollback)
        .count();
    assert_eq!(rollbacks as u32, ROLLBACK_ATTEMPTS + 1);
}

/// Catches a `cleanup` that fails being rolled back, or retried without end: it is run again
/// `cleanup_retries` times, then `Failed(after_commit)`, and `rollback` never runs.
#[test]
fn a_failing_cleanup_is_retried_forward_and_never_rolled_back() {
    let rig = Rig::new("run-cleanup-fails");
    rig.fake.say(Step::Cleanup, &[FAIL, FAIL, FAIL, FAIL, FAIL]);
    assert_eq!(run(&rig), failed(Phase::AfterCommit, Class::Release));
    let steps = rig.fake.steps();
    assert_eq!(steps.iter().filter(|s| **s == Step::Cleanup).count(), 4);
    assert!(!steps.contains(&Step::Rollback));
    assert_eq!(rig.clock.slept(), [DEFAULT_RETRY_SECS; 3]);

    let rig = Rig::new("run-cleanup-recovers");
    rig.fake.say(Step::Cleanup, &[FAIL, DEFER, OK]);
    assert_eq!(run(&rig), ok());
    assert_eq!(rig.clock.slept(), [DEFAULT_RETRY_SECS, 100]);
}

/// Catches a promotion that is not watched: `verify` reporting `promotion_pending` is run
/// again until it exits `0` without the flag, and the update is then `Succeeded`.
#[test]
fn a_pending_promotion_is_watched_until_it_clears() {
    let rig = Rig::new("run-watch");
    rig.fake.say(Step::Verify, &[PENDING, PENDING, OK]);
    assert_eq!(run(&rig), ok());
    assert_eq!(
        rig.fake.steps(),
        [
            Step::Prepare,
            Step::Cutover,
            Step::Verify,
            Step::Cleanup,
            Step::Verify,
            Step::Verify
        ]
    );
    assert_eq!(rig.clock.slept(), [WATCH_POLL_SECS]);
    let verify_timeouts: Vec<u64> = rig
        .fake
        .runs()
        .iter()
        .filter_map(|e| match e {
            support::Event::Run {
                step: Step::Verify,
                timeout_secs,
                ..
            } => Some(*timeout_secs),
            _ => None,
        })
        .collect();
    assert_eq!(verify_timeouts, [1000, 1000, 1000]);
}

/// Catches a watcher that outlives `promotion_ceiling_secs`, or that rolls back: the ceiling ends it
/// as `Failed(after_commit)` with no `rollback`.
#[test]
fn the_promotion_ceiling_ends_the_watch_without_a_rollback() {
    let rig = Rig::new("run-ceiling");
    let mut order = rig.order();
    order.bounds.promotion_ceiling_secs = 100;
    rig.fake.say(Step::Verify, &[PENDING; 10]);
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(got, failed(Phase::AfterCommit, Class::Release));
    assert!(!rig.fake.steps().contains(&Step::Rollback));
    assert_eq!(rig.clock.slept(), [WATCH_POLL_SECS; 4]);
}

/// Catches a watcher that rolls back or hides what ended it: a failed, deferred or halting
/// `verify` while watching ends it with the hook's class (or a halt), and runs nothing more.
#[test]
fn a_failure_while_watching_is_after_the_commit_and_never_a_rollback() {
    for (reply, want) in [
        (FAIL, failed(Phase::AfterCommit, Class::Release)),
        (DEFER, failed(Phase::AfterCommit, Class::Release)),
        (Reply::Killed, failed(Phase::AfterCommit, Class::Release)),
        (HALT, fin(Verdict::Halt)),
    ] {
        let rig = Rig::new("run-watch-ends");
        rig.fake.say(Step::Verify, &[PENDING, reply.clone()]);
        assert_eq!(run(&rig), want, "{reply:?}");
        assert!(!rig.fake.steps().contains(&Step::Rollback));
    }
    let rig = Rig::new("run-watch-env");
    rig.fake.say(Step::Verify, &[PENDING, ENV_FAIL]);
    assert_eq!(
        run(&rig),
        Finished {
            verdict: Verdict::Failed {
                phase: Phase::AfterCommit,
                class: Class::Environment
            },
            reason: Some("remote_down".to_owned())
        }
    );
}

/// Catches a new order that is refused while only a watcher runs, or that runs beside an update
/// that is under way: an order for another update stops the watcher (and its live `verify`) and
/// starts the next generation; the same order resumes; another order during `cutover` is `Busy`.
#[test]
fn a_new_order_stops_the_watcher_but_not_an_update() {
    let rig = Rig::new("run-new-order");
    rig.fake.say(Step::Verify, &[PENDING, PENDING]);
    // Run 5 is the first `verify` of the watcher: the updater dies there.
    rig.fake
        .lose_run
        .store(5, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        rig.runner().run(&rig.order()),
        Err(RunError::Launch(_))
    ));
    let mut next = rig.order();
    next.target_version = "3.0.0".to_owned();
    rig.fake.clear();
    assert_eq!(rig.runner().run(&next).unwrap(), ok());
    assert_eq!(rig.fake.stops(), ["updater-svc-verify-1"]);
    let units: Vec<String> = rig
        .fake
        .runs()
        .iter()
        .filter_map(|e| match e {
            support::Event::Run { unit, .. } => Some(unit.clone()),
            support::Event::Stop(_) => None,
        })
        .collect();
    assert_eq!(units[0], "updater-svc-prepare-2");

    let rig = Rig::new("run-busy");
    rig.fake
        .lose_run
        .store(2, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.runner().run(&rig.order()).is_err());
    let mut other = rig.order();
    other.target_version = "3.0.0".to_owned();
    let err = rig.runner().run(&other).unwrap_err();
    assert!(
        matches!(&err, RunError::Busy { service, target } if service == "svc" && target == "2.0.0")
    );
    assert!(err.to_string().contains("not finished"));
    assert_eq!(rig.runner().run(&rig.order()).unwrap(), ok());
}

/// Catches an order for an updater that is too old being run: it is `Deferred` as `updater_old`,
/// nothing runs and no record is made; an order the updater satisfies runs.
#[test]
fn a_hook_that_needs_a_newer_updater_is_deferred() {
    let rig = Rig::new("run-protocol");
    let mut order = rig.order();
    order.min_updater_protocol = Some(kbf_step_runner::runner::UPDATER_PROTOCOL + 1);
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(
        got,
        Finished {
            verdict: Verdict::Deferred {
                retry_after_secs: None
            },
            reason: Some("updater_old".to_owned())
        }
    );
    assert!(rig.fake.events().is_empty());
    assert_eq!(rig.runner().last_result().unwrap(), None);
    assert!(!rig.state.join(RECORD_FILE).exists());
    order.min_updater_protocol = Some(kbf_step_runner::runner::UPDATER_PROTOCOL);
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
}

/// Catches an order whose names could reach a unit name or a path unchecked: a service name
/// outside letters, digits, `-` and `_`, and a release path that is not UTF-8 (the params file
/// and the artifact argument must be the same string).
#[test]
fn an_order_with_unusable_names_is_refused() {
    let rig = Rig::new("run-bad-order");
    for name in ["", "a b", "a/b", "ünï", "a.b"] {
        let mut order = rig.order();
        order.service = name.to_owned();
        let err = rig.runner().run(&order).unwrap_err();
        assert!(matches!(err, RunError::BadOrder(_)), "{name}");
        assert!(err.to_string().contains("not valid"));
    }
    use std::os::unix::ffi::OsStringExt;
    let odd = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
    let mut order = rig.order();
    order.staged = Staged {
        dir: odd.clone(),
        digest: "x".to_owned(),
    };
    assert!(matches!(
        rig.runner().run(&order),
        Err(RunError::BadOrder(_))
    ));
    let mut order = rig.order();
    order.previous = Some(Staged {
        dir: odd,
        digest: "x".to_owned(),
    });
    assert!(matches!(
        rig.runner().run(&order),
        Err(RunError::BadOrder(_))
    ));
    assert!(rig.fake.events().is_empty());
}

/// Catches an artifact altered after it was verified being run: a tree changed before the first
/// step halts the update with nothing run.
#[test]
fn an_artifact_altered_after_verification_never_runs() {
    let rig = Rig::new("run-altered");
    let order = rig.order();
    fs::write(rig.new_dir.join("lib/data"), "tampered").unwrap();
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(
        got,
        Finished {
            verdict: Verdict::Halt,
            reason: Some("artifact_changed".to_owned())
        }
    );
    assert!(rig.fake.events().is_empty());
    assert_eq!(rig.runner().last_result().unwrap(), Some(got));
}

/// Catches the check being made once: a tree altered by a step is caught before the next step
/// runs, and no `rollback` from the altered tree runs either.
#[test]
fn an_artifact_altered_between_steps_halts_before_the_next() {
    let rig = Rig::new("run-altered-between");
    let data = rig.new_dir.join("lib/data");
    rig.fake.on_run(move |call| {
        if call.invocation.step == Step::Cutover {
            fs::write(&data, "tampered").unwrap();
        }
    });
    let got = run(&rig);
    assert_eq!(got.verdict, Verdict::Halt);
    assert_eq!(got.reason.as_deref(), Some("artifact_changed"));
    assert_eq!(rig.fake.steps(), [Step::Prepare, Step::Cutover]);
}

/// Catches the check being skipped on resume: the updater dies in `cutover`, the tree is altered,
/// and the resumed update stops the old unit and then halts without running the step again.
#[test]
fn an_artifact_altered_while_the_updater_was_down_never_runs_on_resume() {
    let rig = Rig::new("run-altered-resume");
    rig.fake
        .lose_run
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let order = rig.order();
    assert!(rig.runner().run(&order).is_err());
    fs::write(rig.new_dir.join("lib/data"), "tampered").unwrap();
    rig.fake.clear();
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(got.verdict, Verdict::Halt);
    assert_eq!(rig.fake.stops(), ["updater-svc-cutover-1"]);
    assert!(rig.fake.steps().is_empty());
}

/// Catches a `rollback` that has no way out when the new hook cannot start: the previous
/// release's hook, from the previous release, runs it; when that cannot start either, or there is no
/// previous release, the host is a `halt`.
#[test]
fn rollback_falls_back_to_the_previous_hook_only_when_the_new_one_cannot_start() {
    // The staged release carries no hook at all.
    let rig = Rig::new("run-fallback-missing");
    fs::remove_file(rig.new_dir.join("hook")).unwrap();
    assert_eq!(run(&rig), failed(Phase::BeforeCommit, Class::Release));
    let runs = rig.fake.runs();
    assert_eq!(runs.len(), 1);
    let support::Event::Run {
        step, executable, ..
    } = &runs[0]
    else {
        unreachable!()
    };
    assert_eq!(
        (*step, executable),
        (Step::Rollback, &rig.old_dir.join("hook"))
    );

    // Neither release has one.
    let rig = Rig::new("run-fallback-neither");
    fs::remove_file(rig.new_dir.join("hook")).unwrap();
    fs::remove_file(rig.old_dir.join("hook")).unwrap();
    assert_eq!(run(&rig), fin(Verdict::Halt));
    assert!(rig.fake.events().is_empty());

    // No previous release.
    let rig = Rig::new("run-fallback-first-install");
    fs::remove_file(rig.new_dir.join("hook")).unwrap();
    let mut order = rig.order();
    order.previous = None;
    assert_eq!(rig.runner().run(&order).unwrap(), fin(Verdict::Halt));

    // The launcher says the new hook did not start in `rollback`.
    let rig = Rig::new("run-fallback-nostart");
    rig.fake.say(Step::Prepare, &[FAIL]);
    rig.fake.say(Step::Rollback, &[Reply::NoStart]);
    assert_eq!(run(&rig), failed(Phase::BeforeCommit, Class::Release));
    let exes: Vec<_> = rig
        .fake
        .runs()
        .iter()
        .filter_map(|e| match e {
            support::Event::Run {
                step: Step::Rollback,
                executable,
                ..
            } => Some(executable.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(exes, [rig.new_dir.join("hook"), rig.old_dir.join("hook")]);

    // A hook that does not start in any other step is a failed step, not a fallback.
    let rig = Rig::new("run-nostart-cutover");
    rig.fake.say(Step::Cutover, &[Reply::NoStart]);
    assert_eq!(run(&rig), failed(Phase::BeforeCommit, Class::Release));
    assert_eq!(
        rig.fake.steps(),
        [Step::Prepare, Step::Cutover, Step::Rollback]
    );
}

/// Catches a hook path that leads out of the release: a declared `binary` of `../x` or an absolute
/// path never runs, and the update fails as a hook that cannot start.
#[test]
fn a_hook_path_outside_the_release_never_runs() {
    let rig = Rig::new("run-hook-path");
    let mut order = rig.order();
    order.hook_binary = PathBuf::from("../old/hook");
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(got, fin(Verdict::Halt));
    assert!(rig.fake.events().is_empty());
    // The halt is the record's verdict, reached through a failed `prepare` and a `rollback` that
    // could not start either.
    assert_eq!(rig.runner().last_result().unwrap(), Some(got));
    let Loaded::Current(rec) = rig.store().load().unwrap() else {
        panic!("a record")
    };
    assert_eq!(rec.step, Step::Rollback);
    assert_eq!(rec.hook, kbf_step_runner::record::HookSel::Previous);
}

/// Catches a launcher failure being swallowed: the runner stops with the error, the step stays
/// `running` in the record, and the next run stops the unit by name before it runs the step
/// again.
#[test]
fn a_launcher_failure_stops_the_runner_and_resume_stops_the_unit_first() {
    let rig = Rig::new("run-launcher-fails");
    rig.fake.say(Step::Cutover, &[Reply::Lost]);
    let err = rig.runner().run(&rig.order()).unwrap_err();
    assert!(matches!(err, RunError::Launch(_)));
    assert!(err.to_string().contains("launcher"));
    let Loaded::Current(rec) = rig.store().load().unwrap() else {
        panic!("a record")
    };
    assert_eq!(rec.step, Step::Cutover);
    assert!(matches!(rec.state, State::Running { .. }));
    assert_eq!(rig.runner().last_result().unwrap(), None);

    rig.fake.clear();
    assert_eq!(run(&rig), ok());
    let events = rig.fake.events();
    assert_eq!(
        events[0],
        support::Event::Stop("updater-svc-cutover-1".to_owned())
    );
    assert_eq!(
        rig.fake.steps(),
        [Step::Cutover, Step::Verify, Step::Cleanup]
    );
    assert_eq!(rig.runner().last_result().unwrap(), Some(ok()));

    // A stop that fails leaves the record as it was.
    let rig = Rig::new("run-stop-fails");
    rig.fake
        .lose_run
        .store(2, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.runner().run(&rig.order()).is_err());
    rig.fake.fail_stops(1);
    assert!(rig.runner().run(&rig.order()).is_err());
    assert_eq!(run(&rig), ok());
}

/// Catches a record of a newer updater being changed or run: it is refused with its schema and
/// the file is not touched.
#[test]
fn a_record_of_a_newer_schema_is_never_run_over() {
    let rig = Rig::new("run-newer");
    let text = r#"{"schema": 99}"#;
    fs::write(rig.state.join(RECORD_FILE), text).unwrap();
    let err = rig.runner().run(&rig.order()).unwrap_err();
    assert!(matches!(err, RunError::RecordNewer(99)));
    assert!(err.to_string().contains("99"));
    assert!(matches!(
        rig.runner().last_result(),
        Err(RunError::RecordNewer(99))
    ));
    assert_eq!(
        fs::read_to_string(rig.state.join(RECORD_FILE)).unwrap(),
        text
    );
    assert!(rig.fake.events().is_empty());
}

/// Catches the verify window surviving a reboot as if the time had passed: the monotonic clock
/// restarts, so a window begun in another boot begins again; in the same boot the time already
/// spent is subtracted.
#[test]
fn the_verify_window_restarts_after_a_reboot() {
    for (reboot, want) in [(false, 400), (true, 1000)] {
        let rig = Rig::new("run-reboot");
        let mut order = rig.order();
        order.bounds.verify_secs = 1000;
        // The updater dies in the first `verify` (run 3).
        rig.fake
            .lose_run
            .store(3, std::sync::atomic::Ordering::SeqCst);
        assert!(rig.runner().run(&order).is_err());
        rig.clock.advance(1200);
        if reboot {
            rig.clock.reboot("boot-b");
        }
        rig.fake.clear();
        assert_eq!(rig.runner().run(&order).unwrap(), ok());
        let timeouts: Vec<u64> = rig
            .fake
            .runs()
            .iter()
            .filter_map(|e| match e {
                support::Event::Run {
                    step: Step::Verify,
                    timeout_secs,
                    ..
                } => Some(*timeout_secs),
                _ => None,
            })
            .collect();
        assert_eq!(timeouts, [want], "reboot: {reboot}");
    }
}

/// Catches a resume that loses the record's memory of the commit: the updater dies after a
/// `cutover` that reported `reversible: false`, and the verdict of the resumed update still
/// says `after_commit`.
#[test]
fn the_commit_survives_a_crash() {
    let rig = Rig::new("run-commit-resume");
    rig.fake.say(Step::Cutover, &[IRREVERSIBLE_OK]);
    rig.fake.say(Step::Verify, &[FAIL, FAIL]);
    rig.fake
        .lose_run
        .store(3, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.runner().run(&rig.order()).is_err());
    let got = rig.runner().run(&rig.order()).unwrap();
    assert_eq!(got, failed(Phase::AfterCommit, Class::Release));
}

/// Catches `Arc` plumbing that gives each runner its own watch: the runner hands out the watch the
/// dead-man must be given.
#[test]
fn the_runner_hands_out_one_watch() {
    let rig = Rig::new("run-watch-handle");
    let runner = rig.runner();
    assert!(Arc::ptr_eq(&runner.watch(), &runner.watch()));
}

/// Catches an update that is taken for the one under way when it is not: an order that differs
/// in the service, the target, the digest of the staged release or of the previous one is not the
/// unfinished update (it is `Busy`), and only the same order resumes it.
#[test]
fn only_the_same_order_resumes_an_unfinished_update() {
    let rig = Rig::new("run-same-order");
    rig.fake
        .lose_run
        .store(2, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.runner().run(&rig.order()).is_err());
    let mut others = Vec::new();
    let mut o = rig.order();
    o.service = "other".to_owned();
    others.push(o);
    let mut o = rig.order();
    o.target_version = "2.0.1".to_owned();
    others.push(o);
    let mut o = rig.order();
    o.staged.digest = "0".repeat(64);
    others.push(o);
    let mut o = rig.order();
    o.previous = None;
    others.push(o);
    let mut o = rig.order();
    o.previous.as_mut().unwrap().digest = "0".repeat(64);
    others.push(o);
    for other in others {
        let err = rig.runner().run(&other).unwrap_err();
        assert!(matches!(err, RunError::Busy { .. }), "{err:?}");
    }
    assert_eq!(rig.runner().run(&rig.order()).unwrap(), ok());
}

/// Catches a new order after a verdict being refused or run as the old generation: an order for
/// another target, or for another artifact, after `Succeeded` or `Failed` starts the next
/// generation (its units are numbered 2).
#[test]
fn a_new_order_after_a_verdict_starts_the_next_generation() {
    let unit_of_first_run = |rig: &Rig| match &rig.fake.runs()[0] {
        support::Event::Run { unit, .. } => unit.clone(),
        support::Event::Stop(_) => unreachable!(),
    };
    let rig = Rig::new("run-generation-ok");
    assert_eq!(run(&rig), ok());
    rig.fake.clear();
    let mut next = rig.order();
    next.target_version = "3.0.0".to_owned();
    assert_eq!(rig.runner().run(&next).unwrap(), ok());
    assert_eq!(unit_of_first_run(&rig), "updater-svc-prepare-2");
    assert!(rig.fake.stops().is_empty());

    let rig = Rig::new("run-generation-failed");
    rig.fake.say(Step::Verify, &[FAIL]);
    assert_eq!(run(&rig), failed(Phase::GateRelease, Class::Release));
    rig.fake.clear();
    fs::write(rig.new_dir.join("lib/data"), "a rebuilt release").unwrap();
    let rebuilt = rig.order();
    assert_eq!(rig.runner().run(&rebuilt).unwrap(), ok());
    assert_eq!(unit_of_first_run(&rig), "updater-svc-prepare-2");
}

/// Catches an identical order delivered again after a verdict being run again: after `Succeeded` or
/// `Failed` it gets the stored verdict and nothing runs. A `Deferred` is the one verdict that is
/// run again, because the order comes back after its wait.
#[test]
fn a_redelivered_order_gets_the_stored_verdict_and_runs_nothing() {
    for (name, verify) in [("ok", OK), ("failed", FAIL)] {
        let rig = Rig::new(&format!("run-redeliver-{name}"));
        rig.fake.say(Step::Verify, &[verify]);
        let order = rig.order();
        let first = rig.runner().run(&order).unwrap();
        rig.fake.clear();
        assert_eq!(rig.runner().run(&order).unwrap(), first);
        assert_eq!(rig.runner().run(&order).unwrap(), first);
        assert!(rig.fake.events().is_empty(), "{name}");
        assert_eq!(rig.runner().last_result().unwrap(), Some(first));
    }
    let rig = Rig::new("run-redeliver-deferred");
    rig.fake.say(Step::Prepare, &[DEFER]);
    let order = rig.order();
    assert!(matches!(
        rig.runner().run(&order).unwrap().verdict,
        Verdict::Deferred { .. }
    ));
    rig.fake.clear();
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
    assert_eq!(rig.fake.steps().len(), 4);
}

/// Catches a halt that a new order or the same one runs over: after `halt` nothing runs for the
/// service, whatever the order, until `clear_halt`; clearing says whether there was a halt, and
/// after it the next order runs.
#[test]
fn a_halt_holds_the_service_until_it_is_cleared() {
    let rig = Rig::new("run-halt-held");
    assert!(!rig.runner().clear_halt().unwrap());
    rig.fake.say(Step::Prepare, &[HALT]);
    let order = rig.order();
    assert_eq!(rig.runner().run(&order).unwrap(), fin(Verdict::Halt));
    rig.fake.clear();
    let mut newer = rig.order();
    newer.target_version = "3.0.0".to_owned();
    for o in [&order, &newer] {
        assert_eq!(rig.runner().run(o).unwrap(), fin(Verdict::Halt));
    }
    assert!(rig.fake.events().is_empty());
    assert!(rig.runner().clear_halt().unwrap());
    assert!(!rig.runner().clear_halt().unwrap());
    assert_eq!(rig.runner().run(&newer).unwrap(), ok());
    assert!(!rig.runner().clear_halt().unwrap());

    let rig = Rig::new("run-halt-clear-newer");
    fs::write(rig.state.join(RECORD_FILE), r#"{"schema": 99}"#).unwrap();
    assert!(matches!(
        rig.runner().clear_halt(),
        Err(RunError::RecordNewer(99))
    ));
}

/// Catches a zero bound handed to a launcher (systemd reads `RuntimeMaxSec=0` as no limit): a
/// `verify` resumed after its window has passed is not started, the update rolls back, and no
/// run is given a bound of 0.
#[test]
fn a_verify_resumed_after_its_window_is_not_started() {
    let rig = Rig::new("run-window-gone");
    let order = rig.order();
    rig.fake
        .lose_run
        .store(3, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.runner().run(&order).is_err());
    rig.clock.advance(2000);
    rig.fake.clear();
    assert_eq!(
        rig.runner().run(&order).unwrap(),
        failed(Phase::GateRelease, Class::Release)
    );
    assert_eq!(rig.fake.steps(), [Step::Rollback]);
    assert_eq!(rig.fake.stops(), ["updater-svc-verify-1"]);
    for e in rig.fake.runs() {
        let support::Event::Run { timeout_secs, .. } = e else {
            unreachable!()
        };
        assert_ne!(timeout_secs, 0);
    }
}

/// Catches a staged release that others can change being run: a tree writable by group halts the
/// update with nothing run (the parents and links are in the artifact tests).
#[test]
fn a_release_in_an_untrusted_place_never_runs() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new("run-untrusted");
    let order = rig.order();
    fs::set_permissions(&rig.new_dir, fs::Permissions::from_mode(0o775)).unwrap();
    let got = rig.runner().run(&order).unwrap();
    assert_eq!(got.verdict, Verdict::Halt);
    assert_eq!(got.reason.as_deref(), Some("artifact_changed"));
    assert!(rig.fake.events().is_empty());
}

/// Catches a params file that is not kept for the whole update or is left behind: it is the
/// path the record names, the same for every step including those after a resume, and gone
/// once the update has ended.
#[test]
fn the_params_file_lives_as_long_as_the_update() {
    let rig = Rig::new("run-params-life");
    rig.fake
        .lose_run
        .store(3, std::sync::atomic::Ordering::SeqCst);
    assert!(rig.runner().run(&rig.order()).is_err());
    let Loaded::Current(rec) = rig.store().load().unwrap() else {
        panic!("a record")
    };
    assert_eq!(rec.params, rig.state.join("params.toml"));
    let before = fs::read_to_string(&rec.params).unwrap();
    rig.fake.clear();
    assert_eq!(run(&rig), ok());
    for e in rig.fake.runs() {
        let support::Event::Run {
            params_file,
            params,
            ..
        } = e
        else {
            unreachable!()
        };
        assert_eq!(params_file, rec.params);
        assert_eq!(params, before);
    }
    assert!(!rec.params.exists());
}

/// Catches a state directory that others can read or write holding the params file: the runner
/// refuses it before anything runs.
#[test]
fn a_state_directory_open_to_others_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new("run-open-state");
    fs::set_permissions(&rig.state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        rig.runner().run(&rig.order()),
        Err(RunError::Record(_))
    ));
    assert!(rig.fake.events().is_empty());
}

/// Catches a service name with `-` or `_` being refused: both are in the name grammar and carry
/// into the unit names.
#[test]
fn a_service_name_may_hold_dashes_and_underscores() {
    let rig = Rig::new("run-name");
    let mut order = rig.order();
    order.service = "my-svc_1".to_owned();
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
    let support::Event::Run { unit, .. } = &rig.fake.runs()[0] else {
        unreachable!()
    };
    assert_eq!(unit, "updater-my-svc_1-prepare-1");
}

/// Catches a params file that names the staged tree instead of the artifact the updater verified:
/// every step of an update, `rollback` included, is given `updater.artifact_digest` as `sha256:`
/// and the digest of the artifact the order names (its tarball, as in SHA256SUMS), which is not the
/// digest of the unpacked tree.
#[test]
fn every_steps_params_file_names_the_verified_artifact() {
    let rig = Rig::new("run-params-digest");
    rig.fake.say(Step::Verify, &[FAIL]);
    let order = rig.order();
    assert_ne!(order.artifact_digest, order.staged.digest);
    rig.runner().run(&order).unwrap();
    let runs = rig.fake.runs();
    assert_eq!(runs.len(), 4);
    assert!(rig.fake.steps().contains(&Step::Rollback));
    for e in runs {
        let support::Event::Run { params, .. } = e else {
            unreachable!()
        };
        let params = Params::parse(&params).unwrap();
        assert_eq!(
            params.artifact_digest().unwrap(),
            Some(format!("sha256:{}", order.artifact_digest).as_str())
        );
    }
}

/// Catches an artifact digest that is not a sha256 being written to the hooks: anything but 64
/// lower-case hex digits is refused before anything runs.
#[test]
fn an_artifact_digest_that_is_not_a_sha256_is_refused() {
    let rig = Rig::new("run-bad-artifact-digest");
    for bad in [
        "",
        "abc",
        &"AB".repeat(32),
        &"zz".repeat(32),
        &"ab".repeat(33),
    ] {
        let mut order = rig.order();
        order.artifact_digest = bad.to_owned();
        assert!(
            matches!(rig.runner().run(&order), Err(RunError::BadOrder(_))),
            "{bad}"
        );
    }
    assert!(rig.fake.events().is_empty());
}

const HUGE_DEFER: Reply = Reply::Exit(75, Some(r#"{"retry_after_secs":1000000000}"#));
const HUGE_PENDING: Reply = Reply::Exit(
    0,
    Some(r#"{"promotion_pending":true,"retry_after_secs":1000000000}"#),
);

/// Catches a hook parking the runner for years: a `retry_after_secs` of a billion is waited as
/// the longest wait the runner allows, after a deferred `rollback`, a deferred `cleanup` and a
/// pending promotion.
#[test]
fn a_huge_retry_after_is_clamped() {
    let max = kbf_step_runner::runner::MAX_RETRY_SECS;
    let rig = Rig::new("run-clamp-rollback");
    rig.fake.say(Step::Prepare, &[FAIL]);
    rig.fake.say(Step::Rollback, &[HUGE_DEFER, OK]);
    run(&rig);
    assert_eq!(rig.clock.slept(), [max]);

    let rig = Rig::new("run-clamp-cleanup");
    rig.fake.say(Step::Cleanup, &[HUGE_DEFER, OK]);
    assert_eq!(run(&rig), ok());
    assert_eq!(rig.clock.slept(), [max]);

    let rig = Rig::new("run-clamp-watch");
    rig.fake.say(Step::Verify, &[PENDING, HUGE_PENDING, OK]);
    assert_eq!(run(&rig), ok());
    assert_eq!(rig.clock.slept(), [max]);
}

/// Catches two tarballs with the same unpacked tree being one update: an order that names
/// another artifact digest is not the unfinished update (`Busy`), and after a verdict it starts
/// the next generation instead of getting the stored verdict.
#[test]
fn another_tarball_is_another_update_even_with_the_same_tree() {
    let rig = Rig::new("run-tarball-busy");
    rig.fake
        .lose_run
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let order = rig.order();
    assert!(rig.runner().run(&order).is_err());
    let mut other = rig.order();
    other.artifact_digest = "cd".repeat(32);
    assert!(matches!(
        rig.runner().run(&other),
        Err(RunError::Busy { .. })
    ));
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
    rig.fake.clear();
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
    assert!(rig.fake.events().is_empty());
    assert_eq!(rig.runner().run(&other).unwrap(), ok());
    let support::Event::Run { unit, .. } = &rig.fake.runs()[0] else {
        unreachable!()
    };
    assert_eq!(unit, "updater-svc-prepare-2");
}

/// Catches a params file left behind when the updater died between the verdict and the removal:
/// the same order delivered again answers with the verdict and removes the file.
#[test]
fn a_redelivered_order_removes_a_params_file_left_behind() {
    let rig = Rig::new("run-params-leftover");
    let order = rig.order();
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
    fs::write(rig.state.join("params.toml"), "left = 1\n").unwrap();
    assert_eq!(rig.runner().run(&order).unwrap(), ok());
    assert!(!rig.state.join("params.toml").exists());
}
