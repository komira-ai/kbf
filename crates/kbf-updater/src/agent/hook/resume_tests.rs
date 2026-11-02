//! A step run again: after it finished, and after it was killed at each write of the record.
//! The record is the hook's memory; every step decides from it, so a rerun reaches the state
//! an uninterrupted run reaches, with the node and the helper where they would be.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use update_hook::{Outcome, Step};

use super::record::Record;
use super::testing::{Ran, Rig, kill_at};
use crate::agent::local::HostState;

const SUCCESS: [Step; 4] = [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup];
const ROLLBACK: [Step; 3] = [Step::Prepare, Step::Cutover, Step::Rollback];

/// What a rig leaves behind: the record, whether the node is drained, and the helper's state.
fn state(rig: &Rig) -> (Option<Record>, bool, HostState) {
    // The fake helper writes `last` on every revert, even one of nothing: it is not part of the
    // state a rerun is held to.
    let mut host = rig.local.world().state.clone();
    host.last = None;
    (rig.record(), rig.drain.is_drained(), host)
}

/// A gate that counts the writes of the record it sees.
fn counting() -> (Arc<dyn Fn() + Send + Sync>, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    (
        Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }),
        count,
    )
}

/// The number of writes each step of `path` makes in an uninterrupted update.
fn writes(path: &[Step]) -> Vec<usize> {
    let rig = Rig::new("hook-count");
    path.iter()
        .map(|step| {
            let (gate, count) = counting();
            assert_eq!(
                rig.run_gated(*step, gate).outcome,
                Outcome::Success,
                "{step}"
            );
            count.load(Ordering::SeqCst)
        })
        .collect()
}

/// Every step run twice in a row: the second run makes no call to the helper or the frontend
/// (a `verify` looks at the health again, which it may), and leaves the record as it was.
///
/// Catches: a step that acts twice, or writes a record that grows or changes on a rerun.
#[test]
fn a_step_run_again_changes_nothing() {
    let rig = Rig::new("hook-twice");
    for step in SUCCESS {
        assert_eq!(rig.run(step).outcome, Outcome::Success, "{step}");
        let (before, calls) = (rig.record(), rig.calls().len());
        let again = rig.run(step);
        assert_eq!(again.outcome, Outcome::Success, "{step} again");
        assert_eq!(rig.record(), before, "{step} again");
        let new: Vec<String> = rig.calls().into_iter().skip(calls).collect();
        match step {
            Step::Verify => assert_eq!(new.len(), 1, "a verify looks at the health"),
            _ => assert!(new.is_empty(), "{step} again made calls: {new:?}"),
        }
    }
    let rig = Rig::new("hook-twice-rollback");
    for step in ROLLBACK {
        assert_eq!(rig.run(step).outcome, Outcome::Success, "{step}");
    }
    let (before, calls) = (rig.record(), rig.calls().len());
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!((rig.record(), rig.calls().len()), (before, calls));
}

/// `cleanup` says the same on a rerun as the first time: the confirm is the commit, so it
/// reports `reversible: false` both times.
///
/// Catches: a rerun that reports a committed update as one that can still go back.
#[test]
fn a_cleanup_run_again_still_says_it_committed() {
    let rig = Rig::new("hook-twice-cleanup");
    for step in SUCCESS {
        rig.run(step);
    }
    assert!(!rig.run(Step::Cleanup).report.reversible);
}

/// A step killed at any write of the record and run again, then the steps after it, reach the
/// record, the node and the helper an uninterrupted update reaches. The kill is a panic in the
/// gate before the write: the write does not happen, as when a process is killed there.
///
/// Catches: a mark written after its act, an act that cannot be run twice, a record that
/// depends on how many times a step ran.
#[test]
fn a_success_killed_at_any_write_and_run_again_reaches_the_same_state() {
    let uninterrupted = Rig::new("hook-kill-want");
    for step in SUCCESS {
        assert_eq!(uninterrupted.run(step).outcome, Outcome::Success);
    }
    let want = state(&uninterrupted);
    for (index, count) in writes(&SUCCESS).into_iter().enumerate() {
        assert!(count > 0, "{} writes nothing", SUCCESS[index]);
        for n in 1..=count {
            let rig = Rig::new("hook-kill");
            for step in &SUCCESS[..index] {
                assert_eq!(rig.run(*step).outcome, Outcome::Success);
            }
            let killed = catch_unwind(AssertUnwindSafe(|| {
                rig.run_gated(SUCCESS[index], kill_at(n));
            }));
            assert!(
                killed.is_err(),
                "{} was not stopped at write {n}",
                SUCCESS[index]
            );
            for step in &SUCCESS[index..] {
                assert_eq!(
                    rig.run(*step).outcome,
                    Outcome::Success,
                    "{step} after a kill at {n}"
                );
            }
            assert_eq!(state(&rig), want, "{} killed at write {n}", SUCCESS[index]);
        }
    }
}

/// The same for the road back: `prepare` and `cutover`, then a `rollback` killed at any write
/// and run again.
///
/// Catches: a rollback killed between the revert and the undrain that gives the node back
/// twice, or never.
#[test]
fn a_rollback_killed_at_any_write_and_run_again_reaches_the_same_state() {
    let uninterrupted = Rig::new("hook-kill-rollback-want");
    for step in ROLLBACK {
        assert_eq!(uninterrupted.run(step).outcome, Outcome::Success);
    }
    let want = state(&uninterrupted);
    let count = writes(&ROLLBACK)[2];
    assert!(count > 0);
    for n in 1..=count {
        let rig = Rig::new("hook-kill-rollback");
        for step in &ROLLBACK[..2] {
            assert_eq!(rig.run(*step).outcome, Outcome::Success);
        }
        let killed = catch_unwind(AssertUnwindSafe(|| {
            rig.run_gated(Step::Rollback, kill_at(n));
        }));
        assert!(killed.is_err(), "rollback was not stopped at write {n}");
        assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
        assert_eq!(state(&rig), want, "rollback killed at write {n}");
    }
}

/// Runs `before` on a rig `setup` has been applied to, then `last` with a gate that makes the
/// `n`th write of the record fail: the temporary file's name is taken by a directory.
fn run_with_failing_write(
    setup: &dyn Fn(&Rig),
    before: &[Step],
    last: Step,
    n: usize,
) -> (Rig, Ran) {
    let rig = Rig::new("hook-unwritable");
    setup(&rig);
    for step in before {
        assert_eq!(rig.run(*step).outcome, Outcome::Success, "{step}");
    }
    let tmp = rig.record_dir().join("record.json.tmp");
    let calls = Arc::new(AtomicUsize::new(0));
    let gate: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if calls.fetch_add(1, Ordering::SeqCst) + 1 == n {
            std::fs::create_dir(&tmp).expect("a directory in the way");
        }
    });
    let ran = rig.run_gated(last, gate);
    (rig, ran)
}

/// The writes of the record `last` makes, after `before` succeeded on a rig `setup` was applied to.
fn writes_after(setup: &dyn Fn(&Rig), before: &[Step], last: Step) -> usize {
    let rig = Rig::new("hook-writes");
    setup(&rig);
    for step in before {
        assert_eq!(rig.run(*step).outcome, Outcome::Success, "{step}");
    }
    let (gate, count) = counting();
    rig.run_gated(last, gate);
    count.load(Ordering::SeqCst)
}

/// A write of the record that fails fails the step, at whichever write it is: nothing goes on
/// past a mark that was not kept. On the road of a successful update, on the road back, and on
/// the roads that end in a failure of their own (a drain the frontend refuses, a confirm the
/// helper refuses, a trial taken over, a host that runs the release already).
///
/// Catches: a step that carries on after its mark could not be written.
#[test]
fn a_record_that_cannot_be_written_fails_the_step() {
    use crate::agent::local::{LocalError, Trial};
    use crate::agent::testing::{COMMIT_B, VA, VB, version};
    use crate::ports::PortError;

    type Setup = fn(&Rig);
    let nothing: Setup = |_| {};
    let mut scenarios: Vec<(&str, Setup, Vec<Step>, Step)> = Vec::new();
    for path in [SUCCESS.as_slice(), ROLLBACK.as_slice()] {
        for index in 0..path.len() {
            scenarios.push(("the road", nothing, path[..index].to_vec(), path[index]));
        }
    }
    scenarios.push(("not switched", nothing, vec![Step::Prepare], Step::Rollback));
    scenarios.push((
        "a drain refused",
        |rig| {
            *rig.drain.drain_error.lock().unwrap() = Some(PortError::other("404"));
        },
        vec![],
        Step::Prepare,
    ));
    scenarios.push((
        "a confirm refused",
        |rig| {
            rig.local.world().confirm = Err(LocalError::Failed("state_unavailable".to_owned()));
        },
        vec![Step::Prepare, Step::Cutover, Step::Verify],
        Step::Cleanup,
    ));
    scenarios.push((
        "a trial taken over",
        |rig| {
            let mut world = rig.local.world();
            world.state.version = Some(version(VB));
            world.state.commit = Some(COMMIT_B.to_owned());
            world.state.pending = Some(Trial {
                version: Some(version(VB)),
                commit: COMMIT_B.to_owned(),
                deadline_unix: Some(10_400),
            });
            world.before = Some(version(VA));
        },
        vec![],
        Step::Prepare,
    ));
    scenarios.push((
        "a release that runs",
        |rig| {
            rig.local.world().state.version = Some(version(VB));
        },
        vec![],
        Step::Prepare,
    ));
    scenarios.push((
        "a release that runs, verified",
        |rig| {
            rig.local.world().state.version = Some(version(VB));
        },
        vec![Step::Prepare],
        Step::Verify,
    ));
    scenarios.push((
        "a release staged as current",
        |rig| {
            rig.local.world().install = Ok(crate::agent::local::Staged {
                commit: COMMIT_B.to_owned(),
                current: true,
            });
        },
        vec![],
        Step::Prepare,
    ));
    for (name, setup, before, last) in scenarios {
        let count = writes_after(&setup, &before, last);
        assert!(count > 0, "{name}: {last} writes nothing");
        for n in 1..=count {
            let (_, ran) = run_with_failing_write(&setup, &before, last, n);
            assert_eq!(ran.outcome, Outcome::Failure, "{name}: {last} at write {n}");
            assert!(
                ["record_unwritable", "confirm_refused"].contains(&ran.reason()),
                "{name}: {last} at write {n}: {}",
                ran.reason()
            );
        }
    }
}
