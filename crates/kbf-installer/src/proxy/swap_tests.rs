//! The frontend swap against the fakes of [`super::fakes`]. Each test names the defect it
//! catches; the mutants that were planted to see it red are listed in the PR.

use std::time::Duration;

use super::fakes::*;
use super::model::*;
use super::swap::Swap;
use crate::testkit::{B, c};

fn go(w: &World, plan: Plan, old: ProcIdent) -> Result<Phase, Halt> {
    Swap::begin(w.ports(), plan, Some(old))?.run()
}

/// `second` happens after the first `first`: the log holds the two in this order.
fn before(w: &World, first: &str, second: &str) {
    seq(w, &[first, second]);
}

/// The log holds these events in this order, not necessarily next to each other; each
/// is matched at the earliest place after the one before it.
fn seq(w: &World, steps: &[&str]) {
    let log = w.log();
    let mut from = 0;
    for step in steps {
        match log[from..].iter().position(|l| l.starts_with(step)) {
            Some(i) => from += i + 1,
            None => panic!("`{step}` does not follow the earlier steps {steps:?}, log: {log:#?}"),
        }
    }
}

fn untouched(w: &World, pid: u32) {
    let hits: Vec<String> = w
        .log()
        .into_iter()
        .filter(|l| l.contains(&format!("pid {pid}")))
        .collect();
    assert!(hits.is_empty(), "pid {pid} was signalled: {hits:?}");
}

/// A world where the old child drains late: permits, streams and sessions clear one after
/// the other.
fn draining_world() -> (World, ProcIdent) {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 3, Some(secs(400)));
    w.busy(Slot::A, 2, Some(secs(500)));
    w.sessions(Slot::A, 1, Some(secs(600)));
    (w, old)
}

#[test]
fn a_swap_runs_every_phase_in_order_and_retires_the_old_child_last() {
    let (w, old) = draining_world();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert_eq!(
        w.phases(),
        vec![
            Phase::Staged,
            Phase::Preflight,
            Phase::Starting,
            Phase::Ready,
            Phase::Holding,
            Phase::Quiescing,
            Phase::Switching,
            Phase::Switched,
            Phase::Baking,
            Phase::Nodes,
            Phase::Draining,
            Phase::Done
        ]
    );
    before(&w, "migrate", "spawn B");
    before(&w, "spawn B", "hold");
    before(&w, "hold", "admin:quiesce A");
    before(&w, "admin:quiesce A", "admin:takeover B");
    before(&w, "admin:takeover B", "release_into B");
    before(&w, "release_into B", "admin:stand_down A");
    before(&w, "admin:stand_down A", "order confirm");
    before(&w, "order confirm", "admin:desired B");
    before(&w, "admin:desired B", "terminate pid 100");
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.count("kill"), 0);
    assert!(!w.alive(100) && w.alive(101));
}

#[test]
fn the_old_child_is_retired_only_when_permits_streams_and_sessions_are_all_zero() {
    let (w, old) = draining_world();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    let at = w.when("terminate pid 100").expect("retired");
    assert!(
        at >= secs(600),
        "retired at {at:?}, before the sessions closed at 600 s"
    );
    assert_eq!(w.count("terminate"), 1);
}

#[test]
fn an_old_child_that_never_drains_is_never_signalled_in_24_hours() {
    for (name, setup) in [
        (
            "a permit stays open",
            Box::new(|w: &World| w.permits(Slot::A, 1, None)) as Box<dyn Fn(&World)>,
        ),
        (
            "a stream stays open",
            Box::new(|w: &World| w.busy(Slot::A, 1, None)),
        ),
        (
            "a worker session stays open",
            Box::new(|w: &World| w.sessions(Slot::A, 1, None)),
        ),
        (
            "the gate never opened the slot, so it cannot be retired",
            Box::new(|w: &World| w.st.borrow_mut().never_opened.push(Slot::A)),
        ),
        (
            "the child cannot be asked",
            Box::new(|w: &World| w.st.borrow_mut().open_counts_err = true),
        ),
    ] {
        let (w, old) = World::with_old();
        setup(&w);
        w.set_limit(secs(24 * 3600 + 600));
        assert_eq!(
            go(&w, plan(Mode::Frontend), old),
            Err(Halt::Interrupted),
            "{name}"
        );
        assert!(
            w.elapsed() > secs(24 * 3600),
            "{name}: waited {:?}",
            w.elapsed()
        );
        assert_eq!(w.count("terminate"), 0, "{name}");
        assert_eq!(w.count("kill"), 0, "{name}");
    }
}

#[test]
fn the_old_child_is_not_signalled_at_the_flip() {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 1, Some(secs(2000)));
    w.set_limit(secs(1000));
    assert_eq!(go(&w, plan(Mode::Frontend), old), Err(Halt::Interrupted));
    assert!(w.pos("release_into B").is_some(), "the swap happened");
    untouched(&w, 100);
}

#[test]
fn an_old_child_that_exits_by_itself_while_draining_ends_the_update() {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 5, None);
    w.script(Script::default());
    let mut s = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old)).unwrap();
    // The old child dies once the swap has switched: its open streams are lost.
    w.st.borrow_mut().procs[0].script.exit_after = Some(secs(400));
    assert_eq!(s.run(), Ok(Phase::Done));
    assert_eq!(w.count("terminate"), 0);
}

#[test]
fn a_candidate_that_is_never_ready_is_killed_and_the_old_child_is_untouched() {
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: None,
    });
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    let intent = w.last_intent();
    assert_eq!(intent.failure.as_ref().map(|f| f.step), Some(Step::Ready));
    assert!(
        w.elapsed() >= secs(120) && w.elapsed() < secs(125),
        "{:?}",
        w.elapsed()
    );
    assert!(!w.alive(101), "the candidate was killed");
    assert_eq!(w.count("kill pid 101"), 1);
    // No admin call, hold or signal touched the old child.
    assert!(
        w.log()
            .iter()
            .all(|l| !l.starts_with("admin:") && l != "hold"),
        "{:#?}",
        w.log()
    );
    untouched(&w, 100);
    assert!(w.alive(100) && w.holders() == vec![Slot::A]);
    assert_eq!(w.count("order revert"), 1, "the staged release is dropped");
}

#[test]
fn a_candidate_that_exits_early_is_respawned_once_then_the_update_fails() {
    let (w, old) = World::with_old();
    let dies = Script {
        ready_after: None,
        exit_after: Some(secs(5)),
    };
    w.script(dies);
    w.script(dies);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.count("spawn"), 2);
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Start));
    untouched(&w, 100);
}

#[test]
fn a_respawned_candidate_that_comes_up_carries_the_update_on() {
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: Some(secs(5)),
    });
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert_eq!(w.count("spawn"), 2);
}

#[test]
fn a_failed_migration_starts_no_candidate_and_touches_nothing() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().migrate_err = Some("lock timeout".into());
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    let f = w.last_intent().failure.expect("failure");
    assert_eq!(f.step, Step::Migrate);
    assert!(f.reason.contains("lock timeout"));
    assert_eq!(w.count("spawn"), 0, "no candidate was started");
    assert!(
        w.log()
            .iter()
            .all(|l| !l.starts_with("admin:") && l != "hold")
    );
    assert_eq!(w.count("order revert"), 1);
}

#[test]
fn the_candidate_starts_only_after_the_migration_is_applied() {
    let (w, old) = World::with_old();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    before(&w, "migrate", "spawn B");
}

#[test]
fn a_staged_release_that_is_not_the_updates_fails_before_anything_runs() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().staged = Some(c("1111111111111111111111111111111111111111"));
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Stage));
    assert_eq!(w.count("migrate"), 0);
    let _ = B;
}

#[test]
fn a_failed_takeover_kills_the_candidate_then_reads_roles_then_resumes_the_old_child() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().takeover = Takeover::Err;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Switch));
    // One chain from the takeover on: the role read that decides the resume is the one
    // after the kill (quiesce polling read `role A` long before).
    seq(
        &w,
        &[
            "admin:takeover B",
            "kill pid 101",
            "admin:role A",
            "admin:resume A",
            "release_into A",
        ],
    );
    assert_eq!(w.count("release_into B"), 0, "nothing was swapped");
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(w.state_of(100), RoleState::Active);
}

#[test]
fn a_takeover_over_ten_seconds_is_a_failed_switch_even_if_it_succeeded() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().takeover = Takeover::Slow(secs(11));
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Switch));
    before(&w, "kill pid 101", "admin:resume A");
    assert_eq!(w.count("release_into B"), 0);
    assert_eq!(
        w.holders(),
        vec![Slot::A],
        "the old child re-took the generation"
    );
}

#[test]
fn a_quiesce_over_three_seconds_is_a_failed_switch_and_takeover_is_never_asked() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(4);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.count("admin:takeover"), 0);
    before(&w, "kill pid 101", "admin:resume A");
    assert_eq!(w.count("release_into A"), 1);
}

#[test]
fn the_hold_is_released_into_the_new_child_only_after_takeover_resolved() {
    let (w, old) = World::with_old();
    w.hold_streams(200);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    before(&w, "admin:takeover B", "release_into B");
    assert_eq!(w.st.borrow().fed, vec![(Slot::B, 200)]);
    assert_eq!(w.st.borrow().expired, 0);
}

#[test]
fn an_operator_abort_while_the_candidate_starts_kills_it_and_leaves_the_old_child() {
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: None,
    });
    w.request_at(secs(10), Request::Abort);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Abort));
    assert_eq!(w.count("kill pid 101"), 1);
    assert!(w.elapsed() < secs(20));
    untouched(&w, 100);
}

#[test]
fn the_bake_confirms_after_the_window_and_posts_desired_only_if_asked() {
    let (w, old) = World::with_old();
    let mut p = plan(Mode::Frontend);
    p.nodes = false;
    assert_eq!(go(&w, p, old), Ok(Phase::Done));
    assert!(w.when("order confirm").unwrap() >= secs(300));
    assert_eq!(w.count("admin:desired"), 0);
    assert!(!w.phases().contains(&Phase::Nodes));
}

#[test]
fn a_failed_desired_post_does_not_fail_the_frontend() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().desired_err = true;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert!(!w.last_intent().nodes_posted);
}

#[test]
fn a_confirm_that_keeps_failing_keeps_the_new_child_serving_and_retires_the_old_one() {
    let (w, old) = draining_world();
    w.st.borrow_mut().confirm_err = true;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    let f = w.last_intent().failure.expect("failure surfaced");
    assert_eq!(f.step, Step::Confirm);
    assert!(f.reason.contains("confirm refused"), "{}", f.reason);
    assert_eq!(w.count("order confirm"), 6, "six tries");
    // The waits double from the poll interval: 1, 2, 4, 8, 16 s between six tries.
    let times: Vec<u64> =
        w.st.borrow()
            .events
            .iter()
            .filter(|e| e.1.starts_with("order confirm"))
            .map(|e| (e.0 - START).as_secs())
            .collect();
    let gaps: Vec<u64> = times.windows(2).map(|p| p[1] - p[0]).collect();
    assert_eq!(gaps, vec![1, 2, 4, 8, 16]);
    // The new child served: the staged release stays, nothing is stood back up, the
    // old child is still drained and retired, the nodes are not sent.
    assert_eq!(w.count("order revert"), 0, "staged is not dropped");
    assert_eq!(w.count("admin:resume"), 0);
    assert_eq!(w.count("kill"), 0);
    assert_eq!(
        w.count("admin:desired"),
        0,
        "no nodes under an unconfirmed release"
    );
    assert!(w.when("terminate pid 100").expect("old retired") >= secs(600));
    assert!(!w.alive(100) && w.alive(101));
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.phases().last(), Some(&Phase::Failed));
    assert!(w.phases().contains(&Phase::Draining));
}

#[test]
fn a_confirm_that_fails_twice_then_lands_ends_done_with_no_failure_left() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().confirm_fails = 2;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert_eq!(w.count("order confirm"), 3);
    assert_eq!(w.last_intent().failure, None, "a landed confirm clears it");
    assert_eq!(w.count("admin:desired"), 1);
    assert_eq!(w.count("order revert"), 0);
}

#[test]
fn a_candidate_missing_any_one_readiness_check_is_not_ready() {
    for (gap, name) in [(0, "readyz"), (1, "health"), (2, "capabilities")] {
        let (w, old) = World::with_old();
        w.st.borrow_mut().ready_gap = Some(gap);
        assert_eq!(
            go(&w, plan(Mode::Frontend), old),
            Ok(Phase::Failed),
            "{name}"
        );
        assert_eq!(
            w.last_intent().failure.map(|f| f.step),
            Some(Step::Ready),
            "{name}"
        );
        assert_eq!(w.count("kill pid 101"), 1, "{name}");
        assert_eq!(w.count("admin:quiesce"), 0, "{name}");
    }
}

#[test]
fn a_refused_hold_fails_the_update_before_the_old_child_is_touched() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().hold_fails_from = Some(1);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Hold));
    assert_eq!(
        w.count("admin:quiesce"),
        0,
        "the old child was never quiesced"
    );
    assert_eq!(w.count("admin:takeover"), 0);
    assert_eq!(w.count("kill pid 101"), 1, "the candidate is killed");
    assert_eq!(w.holders(), vec![Slot::A]);
}

#[test]
fn an_old_child_that_never_quiesces_is_given_up_on_after_three_seconds() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(3600);
    w.set_limit(secs(600));
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    let f = w.last_intent().failure.expect("failure");
    assert_eq!(f.step, Step::Switch);
    assert!(f.reason.contains("did not quiesce in 3 s"), "{}", f.reason);
    assert!(w.elapsed() < secs(10), "gave up at {:?}", w.elapsed());
    assert_eq!(w.count("admin:takeover"), 0);
    seq(
        &w,
        &[
            "kill pid 101",
            "admin:role A",
            "admin:resume A",
            "release_into A",
        ],
    );
}

#[test]
fn an_abort_while_the_old_child_quiesces_is_reported_as_abort_not_switch() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = Duration::from_millis(2500);
    w.request_at(Duration::from_millis(2150), Request::Abort);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Abort));
    assert_eq!(w.count("admin:takeover"), 0);
    seq(
        &w,
        &[
            "admin:quiesce A",
            "kill pid 101",
            "admin:resume A",
            "release_into A",
        ],
    );
}

#[test]
fn a_candidate_that_exits_after_the_thirty_second_window_is_not_respawned() {
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: Some(secs(40)),
    });
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.count("spawn"), 1);
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Start));
    assert!(
        w.elapsed() < secs(60),
        "failed at once, at {:?}",
        w.elapsed()
    );
}

#[test]
fn a_rollback_takes_the_hold_before_it_moves_either_child() {
    let (w, old) = World::with_old();
    w.request_at(secs(60), Request::Rollback);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::RolledBack));
    assert_eq!(w.count("hold"), 2, "the swap's hold and the rollback's");
    // After `switched` was recorded: hold, then both children, then the release.
    seq(
        &w,
        &[
            "record Baking",
            "hold",
            "admin:stand_down B",
            "admin:resume A",
            "release_into A",
        ],
    );
}

#[test]
fn a_rollback_whose_hold_is_refused_changes_nothing_and_the_bake_goes_on() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().hold_fails_from = Some(2);
    w.request_at(secs(60), Request::Rollback);
    let mut s = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old)).unwrap();
    assert_eq!(s.run(), Ok(Phase::Done));
    assert!(s.notes().iter().any(|n| n.contains("the hold failed")));
    assert_eq!(w.count("admin:stand_down B"), 0);
    assert_eq!(w.count("admin:resume"), 0);
    assert!(!w.last_intent().rolling_back);
    assert_eq!(w.holders(), vec![Slot::B]);
}

#[test]
fn the_old_child_is_stood_down_once_and_only_after_the_takeover() {
    let (w, old) = World::with_old();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    assert_eq!(w.count("admin:stand_down A"), 1);
    seq(&w, &["admin:takeover B", "admin:stand_down A"]);
    // A takeover that fails never stands the old child down: it resumes.
    let (w, old) = World::with_old();
    w.st.borrow_mut().takeover = Takeover::Err;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.count("admin:stand_down A"), 0);
    assert_eq!(w.state_of(100), RoleState::Active);
}

#[test]
fn held_streams_that_waited_out_the_hold_fail_before_the_release_feeds_the_rest() {
    let (w, old) = World::with_old();
    // 9 s old at the start: 11 s when the quiesce wait ends, 16 s after a 5 s takeover,
    // so only the check at the release can fail them.
    w.hold_streams_aged(30, secs(9));
    w.st.borrow_mut().takeover = Takeover::Slow(secs(5));
    w.hold_streams(5);
    let mut s = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old)).unwrap();
    assert_eq!(s.run(), Ok(Phase::Done));
    assert_eq!(w.st.borrow().expired, 30);
    assert_eq!(
        w.st.borrow().fed,
        vec![(Slot::B, 5)],
        "only the fresh ones are fed"
    );
    assert!(
        s.notes()
            .iter()
            .any(|n| n.contains("30 held streams expired"))
    );
}

#[test]
fn the_hold_is_expired_while_the_old_child_quiesces_not_only_at_the_release() {
    let (w, old) = World::with_old();
    // 14 s old when the hold starts: they pass hold_max (15 s) during the quiesce wait.
    w.hold_streams_aged(7, secs(12));
    w.st.borrow_mut().quiesce_takes = Duration::from_millis(2500);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    seq(
        &w,
        &["admin:quiesce A", "gate:expired 7", "admin:takeover B"],
    );
}

#[test]
fn a_dead_new_child_makes_the_bake_red_and_it_waits_for_the_operator() {
    let (w, old) = World::with_old();
    let mut s = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old)).unwrap();
    w.script(Script {
        ready_after: Some(secs(2)),
        exit_after: Some(secs(100)),
    });
    w.set_limit(secs(2000));
    assert_eq!(
        s.run(),
        Err(Halt::Interrupted),
        "no automatic rollback, no confirm"
    );
    assert_eq!(w.count("order confirm"), 0);
    assert_eq!(w.last_intent().phase, Phase::Baking);
}

#[test]
fn a_rollback_request_during_the_bake_stands_down_the_new_child_and_resumes_the_old() {
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: Some(secs(2)),
        exit_after: None,
    });
    w.permits(Slot::B, 2, Some(secs(500)));
    w.request_at(secs(60), Request::Rollback);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::RolledBack));
    before(&w, "admin:stand_down B", "admin:resume A");
    before(&w, "admin:resume A", "release_into A");
    before(&w, "release_into A", "order revert");
    assert_eq!(w.count("order confirm"), 0, "current was never touched");
    assert_eq!(w.holders(), vec![Slot::A]);
    let at = w
        .when("terminate pid 101")
        .expect("the new child is retired");
    assert!(at >= secs(500), "retired at {at:?} with permits open");
    untouched(&w, 100);
}

#[test]
fn a_rollback_is_refused_when_the_old_child_is_gone() {
    let (w, old) = World::with_old();
    w.request_at(secs(60), Request::Rollback);
    let mut s = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old)).unwrap();
    w.st.borrow_mut().procs[0].script.exit_after = Some(secs(30));
    assert_eq!(s.run(), Ok(Phase::Done));
    assert!(s.notes().iter().any(|n| n.contains("rollback refused")));
}

#[test]
fn an_old_child_that_cannot_resume_leaves_the_new_child_serving_and_fails_the_rollback() {
    let (w, old) = World::with_old();
    w.request_at(secs(60), Request::Rollback);
    w.st.borrow_mut().resume_fails.push(Slot::A);
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(
        w.last_intent().failure.map(|f| f.step),
        Some(Step::Rollback)
    );
    before(&w, "admin:resume A", "admin:resume B");
    before(&w, "admin:resume B", "release_into B");
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.count("terminate"), 0, "neither child is retired");
}

/// Catches a frontend swap that starts the new child serving: it must wait (`--await-takeover`)
/// so that the takeover, after the old child quiesced, is the only moment it holds the
/// generation.
#[test]
fn the_new_child_of_a_frontend_swap_is_started_to_wait_for_the_takeover() {
    let (w, old) = World::with_old();
    assert_eq!(
        go(&w, plan(Mode::Frontend), old),
        Ok(Phase::Done),
        "{:#?}",
        w.log()
    );
    assert_eq!(w.count("admin:takeover B"), 1, "{:#?}", w.log());
    assert_eq!(w.st.borrow().starts, vec![(Slot::B, Start::Candidate)]);
    assert_eq!(w.holders(), vec![Slot::B]);
}

/// Catches a takeover sent without the plan's limit (the child's own deadline): a swap that
/// passes zero or a constant leaves the child with a bound the plan did not choose, so both
/// the switch and the cold start after a restart must pass `timing.takeover`.
#[test]
fn the_takeover_carries_the_plans_limit() {
    let (w, old) = World::with_old();
    let mut p = plan(Mode::Frontend);
    p.timing.takeover = Duration::from_millis(7_321);
    assert_eq!(go(&w, p, old), Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(
        w.count("admin:takeover B limit 7321 ms"),
        1,
        "{:#?}",
        w.log()
    );
}
