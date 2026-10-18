//! The frontend swap against the fakes of [`super::fakes`]. Each test names the defect it
//! catches; the mutants that were planted to see it red are listed in the PR.

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

/// A world where the old child drains late: permits, executions and queued operations
/// clear one after the other.
fn draining_world() -> (World, ProcIdent) {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 3, Some(secs(400)));
    w.busy(Slot::A, 2, Some(secs(500)));
    w.queued(Slot::A, 1, Some(secs(600)));
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
fn the_old_child_is_retired_only_when_streams_executions_and_queue_are_all_zero() {
    let (w, old) = draining_world();
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Done));
    let at = w.when("terminate pid 100").expect("retired");
    assert!(
        at >= secs(600),
        "retired at {at:?}, before the queue emptied at 600 s"
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
            "an execution stays open",
            Box::new(|w: &World| w.busy(Slot::A, 1, None)),
        ),
        (
            "a queued operation stays",
            Box::new(|w: &World| w.queued(Slot::A, 1, None)),
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
    before(&w, "admin:takeover B", "kill pid 101");
    before(&w, "kill pid 101", "admin:role A");
    before(&w, "admin:role A", "admin:resume A");
    before(&w, "admin:resume A", "release_into A");
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
fn a_failed_confirm_fails_the_update_at_confirm() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().confirm_err = true;
    assert_eq!(go(&w, plan(Mode::Frontend), old), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Confirm));
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
