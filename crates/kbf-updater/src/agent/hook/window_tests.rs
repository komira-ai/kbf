//! The windows around the drain: a deadline that lifts the cordon it placed, a crash between the
//! placement and the drain, and acts whose answer was lost.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use update_hook::{Class, Outcome, Step};

use super::record::Was;
use super::testing::{Rig, kill_at};
use crate::agent::cordon::OwnCordon;
use crate::agent::local::LocalError;
use crate::agent::testing::{ServingNode, VB};
use crate::ports::PortError;
use crate::testutil::scratch;

fn s(text: &str) -> String {
    text.to_owned()
}

fn deadline(rig: &mut Rig) {
    rig.flags.extend([s("--drain-deadline-secs"), s("60")]);
    *rig.drain.leases.lock().unwrap() = [2].into();
}

/// A rig whose drain is the agent's cordon marks over a node the frontend keeps.
fn with_cordon(name: &str) -> (Rig, ServingNode) {
    let mut rig = Rig::new(name);
    let node = ServingNode::new();
    let marks = scratch(&format!("{name}-marks"));
    rig.drain_port = Arc::new(OwnCordon::new(Arc::new(node.clone()), &marks, "kbf"));
    (rig, node)
}

/// A deadline whose `undrain` fails is not a deferral: the step exits 1 with `undrain_failed`
/// (the environment's), the node stays drained, and the next `rollback` gives it back.
///
/// Catches: a deferral reported with its cordon still on, which the updater cannot lift.
#[test]
fn a_deadline_whose_undrain_fails_is_a_failure_that_the_rollback_retries() {
    let mut rig = Rig::new("hook-deadline-undrain");
    deadline(&mut rig);
    *rig.drain.undrain_error.lock().unwrap() = Some(PortError::other("503"));
    let ran = rig.run(Step::Prepare);
    assert_eq!(
        (ran.outcome, ran.reason()),
        (Outcome::Failure, "undrain_failed")
    );
    assert!(rig.drain.is_drained());
    *rig.drain.undrain_error.lock().unwrap() = None;
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert!(!rig.drain.is_drained());
}

/// A `prepare` run again on the record of a deadline drains the node again, and the update then
/// gives it back at its end: the earlier give-back is not the later one's.
///
/// Catches: a node drained by the second attempt and never given back.
#[test]
fn a_node_drained_again_after_a_deadline_is_given_back_again() {
    let mut rig = Rig::new("hook-deadline-again");
    deadline(&mut rig);
    assert_eq!(rig.run(Step::Prepare).outcome, Outcome::Deferred);
    *rig.drain.leases.lock().unwrap() = [0].into();
    for step in [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup] {
        assert_eq!(rig.run(step).outcome, Outcome::Success, "{step}");
    }
    assert!(!rig.drain.is_drained());
    assert_eq!(rig.events.count("undrain"), 2);
}

/// A step killed after the placement was recorded and before the drain, and run again after an
/// operator drained the node, reads the node again: it is the operator's, so the update drains
/// and waits as for any node and never gives it back. Killed after the drain, the node is ours
/// by its mark and is given back; killed with no operator in between, the same.
///
/// Catches: the update uncordoning a node an operator drained in the window between the
/// recorded placement and the drain.
#[test]
fn a_crash_between_the_placement_and_the_drain_does_not_hand_the_node_to_the_update() {
    for (kill, operator, serving_at_end) in [(2, true, false), (2, false, true), (3, false, true)] {
        let (rig, node) = with_cordon("hook-window");
        let killed = catch_unwind(AssertUnwindSafe(|| {
            rig.run_gated(Step::Prepare, kill_at(kill));
        }));
        assert!(killed.is_err());
        if operator {
            node.operator_drains();
        }
        *node.undrain_error.lock().unwrap() = Some(PortError::other("called"));
        if serving_at_end {
            *node.undrain_error.lock().unwrap() = None;
        }
        for step in [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup] {
            assert_eq!(
                rig.run(step).outcome,
                Outcome::Success,
                "{step} {kill} {operator}"
            );
        }
        assert_eq!(node.is_serving(), serving_at_end, "{kill} {operator}");
        let want = if operator { Was::Out } else { Was::In };
        assert_eq!(rig.record().expect("a record").placement, Some(want));
    }
}

/// A drain whose answer was lost (the frontend unreachable, a timeout) may have landed: the mark
/// stays, and `rollback` gives the node back. A drain the frontend definitely refused, or that
/// was never made (connection refused), clears it.
///
/// Catches: a cordon forgotten because the call timed out, which nothing ever lifts.
#[test]
fn a_drain_that_may_have_landed_keeps_its_mark() {
    let rig = Rig::new("hook-drain-lost");
    *rig.drain.drain_error.lock().unwrap() = Some(PortError::unreachable("timed out"));
    let ran = rig.run(Step::Prepare);
    assert_eq!(
        (ran.reason(), ran.report.class),
        ("drain_failed", Class::Environment)
    );
    assert!(rig.record().expect("a record").drain_begun);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.events.count("undrain"), 1);

    let rig = Rig::new("hook-drain-never-made");
    *rig.drain.drain_error.lock().unwrap() = Some(PortError::refused("connection refused"));
    rig.run(Step::Prepare);
    assert!(!rig.record().expect("a record").drain_begun);
    assert_eq!(rig.run(Step::Rollback).outcome, Outcome::Success);
    assert_eq!(rig.events.count("undrain"), 0);
}

/// A confirm whose answer was lost may have landed: `cleanup` keeps the mark, does not revert
/// (a release that may be confirmed is not put back), exits 1 with `confirm_unknown`, and the
/// next `cleanup` looks at the helper and confirms if it must.
///
/// Catches: a release reverted, or confirmed twice, because a socket timed out.
#[test]
fn a_confirm_that_may_have_landed_keeps_its_mark_and_is_not_reverted() {
    let rig = Rig::new("hook-confirm-lost");
    for step in [Step::Prepare, Step::Cutover, Step::Verify] {
        assert_eq!(rig.run(step).outcome, Outcome::Success);
    }
    rig.local.world().confirm = Err(LocalError::Unreachable(s("timed out")));
    let ran = rig.run(Step::Cleanup);
    assert_eq!(
        (ran.outcome, ran.reason(), ran.report.class),
        (Outcome::Failure, "confirm_unknown", Class::Environment)
    );
    assert!(
        !ran.report.reversible,
        "a confirm that may have landed is not undone"
    );
    assert_eq!(rig.events.count("local:revert"), 0);
    let record = rig.record().expect("a record");
    assert!(record.confirm_begun && !record.rolled_back);
    rig.local.world().confirm = Ok(());
    assert_eq!(rig.run(Step::Cleanup).outcome, Outcome::Success);
    assert_eq!(rig.events.count(&format!("local:confirm {VB}")), 2);
    assert!(!rig.drain.is_drained());
}
