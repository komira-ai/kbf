//! A proxy restart at every phase boundary and at the three named crash points: one test
//! per row of FP section 6. The world outlives the proxy (children are adopted), the
//! proxy's own memory (the hold) does not. A restart reads `update.json` as the recorder
//! last persisted it, then the store decides who serves.

use super::fakes::*;
use super::model::*;
use super::swap::Swap;

fn crash_when(w: &World, f: impl Fn(&Intent) -> bool + 'static) {
    *w.crash_on_record.borrow_mut() = Some(Box::new(f));
}

/// Run until the proxy dies.
fn run_to_crash(w: &World, mode: Mode, old: ProcIdent) {
    let r = Swap::begin(w.ports(), plan(mode), Some(old)).and_then(|mut s| s.run());
    assert!(
        matches!(r, Err(Halt::Crashed(_))),
        "expected a crash, got {r:?}"
    );
}

/// A new proxy adopts the recorded update and runs it on.
fn restart(w: &World) -> (Result<Phase, Halt>, Vec<String>) {
    w.proxy_restart();
    let intent = w.last_intent();
    let timing = match intent.mode {
        Mode::Frontend => Timing::frontend(),
        Mode::AllInOne => Timing::all_in_one(),
    };
    let mut swap = Swap::adopt(w.ports(), Plan::resumed(&intent, timing), intent);
    let end = swap.run();
    (end, swap.notes().to_vec())
}

fn draining_world() -> (World, ProcIdent) {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 3, Some(secs(400)));
    w.busy(Slot::A, 2, Some(secs(500)));
    w.queued(Slot::A, 1, Some(secs(600)));
    (w, old)
}

fn failed_at(w: &World) -> Option<Step> {
    w.last_intent().failure.map(|f| f.step)
}

#[test]
fn a_restart_at_every_phase_boundary_ends_with_exactly_one_serving_child() {
    let phases = [
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
    ];
    for phase in phases {
        let (w, old) = draining_world();
        w.hold_streams(10);
        crash_when(&w, move |i| i.phase == phase);
        run_to_crash(&w, Mode::Frontend, old);
        let (end, _) = restart(&w);
        let pre_switch = phase >= Phase::Starting && phase <= Phase::Switching;
        let want = if pre_switch {
            Phase::Failed
        } else {
            Phase::Done
        };
        assert_eq!(end, Ok(want), "restart after {phase:?}: {:#?}", w.log());
        let holders = w.holders();
        assert_eq!(holders.len(), 1, "after {phase:?}: holders {holders:?}");
        if pre_switch {
            assert_eq!(failed_at(&w), Some(Step::Restart), "{phase:?}");
            assert_eq!(holders, vec![Slot::A], "{phase:?}");
            assert!(
                w.alive(100) && !w.alive(101),
                "{phase:?}: the candidate is killed"
            );
            assert_eq!(w.count("terminate"), 0, "{phase:?}");
        } else {
            assert_eq!(holders, vec![Slot::B], "{phase:?}");
            let at = w.when("terminate pid 100").expect("old child retired");
            assert!(
                at >= secs(600),
                "{phase:?}: retired at {at:?} before it was idle"
            );
            assert!(w.alive(101), "{phase:?}");
        }
        assert_eq!(w.held(), 0, "{phase:?}: the hold died with the proxy");
    }
}

#[test]
fn row_steps_1_to_3_a_restart_before_any_child_started_runs_the_update_again() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Preflight);
    run_to_crash(&w, Mode::Frontend, old);
    assert_eq!(w.count("spawn"), 0);
    assert_eq!(restart(&w).0, Ok(Phase::Done));
    assert_eq!(w.count("spawn"), 1);
}

#[test]
fn row_steps_4_5_a_candidate_that_died_during_the_restart_is_dropped_and_the_old_child_serves() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Starting && i.new_proc.is_some());
    run_to_crash(&w, Mode::Frontend, old);
    w.kill_pid(101);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failed_at(&w), Some(Step::Restart));
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(w.count("kill"), 0, "a dead candidate is not signalled");
    assert_eq!(w.count("release_into A"), 1);
}

#[test]
fn row_step_6_before_takeover_returns_a_takeover_that_did_not_land_resumes_the_old_child() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().takeover = Takeover::NoEffect;
    w.crash_at.set(Some(CrashPoint::BeforeTakeoverReturns));
    run_to_crash(&w, Mode::Frontend, old);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failed_at(&w), Some(Step::Restart));
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(
        w.state_of(100),
        RoleState::Active,
        "quiesced old child resumed"
    );
    assert_eq!(
        w.count("kill pid 101"),
        1,
        "a candidate that is not the holder is killed"
    );
    assert_eq!(w.count("release_into B"), 0);
}

#[test]
fn row_step_6_before_takeover_returns_a_takeover_that_landed_completes_the_swap() {
    let (w, old) = draining_world();
    w.crash_at.set(Some(CrashPoint::BeforeTakeoverReturns));
    run_to_crash(&w, Mode::Frontend, old);
    assert_eq!(
        w.holders(),
        vec![Slot::B],
        "the takeover landed while the proxy was gone"
    );
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done));
    assert_eq!(w.count("kill"), 0);
    assert_eq!(w.count("release_into B"), 1);
    assert_eq!(w.state_of(100), RoleState::StoodDown);
}

#[test]
fn row_step_6_after_takeover_before_the_swap_the_holder_is_active_and_the_old_child_stands_down() {
    let (w, old) = draining_world();
    w.crash_at.set(Some(CrashPoint::AfterTakeoverBeforeSwap));
    run_to_crash(&w, Mode::Frontend, old);
    assert_eq!(w.count("release_into B"), 0, "the swap had not happened");
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done));
    before_in(&w, "release_into B", "admin:stand_down A");
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.count("kill"), 0);
}

#[test]
fn row_proxy_crash_during_the_hold_loses_the_hold_and_the_old_child_serves_again() {
    let (w, old) = World::with_old();
    w.hold_streams(50);
    crash_when(&w, |i| i.phase == Phase::Quiescing);
    run_to_crash(&w, Mode::Frontend, old);
    assert_eq!(w.held(), 50);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(w.held(), 0, "the held streams were cut with the proxy");
    assert_eq!(w.state_of(100), RoleState::Active);
}

#[test]
fn row_proxy_crash_during_bake_continues_the_bake_from_its_recorded_start() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Baking);
    run_to_crash(&w, Mode::Frontend, old);
    let started = w.last_intent().bake_started.expect("bake start recorded");
    // The proxy is down for 100 s of the 300 s bake.
    w.advance(secs(100));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done));
    let confirm = w.when("order confirm").unwrap().as_secs() + START.as_secs();
    assert!(
        (started + 300..started + 303).contains(&confirm),
        "confirm at {confirm}, bake started at {started}"
    );
}

#[test]
fn row_proxy_crash_during_bake_an_expired_bake_passes_at_once() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Baking);
    run_to_crash(&w, Mode::Frontend, old);
    w.advance(secs(1000));
    let before = w.elapsed();
    assert_eq!(restart(&w).0, Ok(Phase::Done));
    assert!(w.when("order confirm").unwrap() < before + secs(2));
}

#[test]
fn row_proxy_crash_during_nodes_posts_desired_again_after_the_restart() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Nodes && i.nodes_posted);
    run_to_crash(&w, Mode::Frontend, old);
    assert_eq!(w.count("admin:desired"), 1);
    assert_eq!(restart(&w).0, Ok(Phase::Done));
    assert_eq!(
        w.count("admin:desired"),
        2,
        "nodes_posted is advisory: the post is repeated"
    );
}

#[test]
fn row_proxy_crash_during_draining_keeps_waiting_on_the_old_childs_counts() {
    let (w, old) = draining_world();
    w.crash_at.set(Some(CrashPoint::WhileDraining));
    run_to_crash(&w, Mode::Frontend, old);
    assert_eq!(w.count("terminate"), 0);
    assert_eq!(restart(&w).0, Ok(Phase::Done));
    assert!(w.when("terminate pid 100").unwrap() >= secs(600));
}

#[test]
fn a_recycled_candidate_pid_is_never_signalled() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Ready);
    run_to_crash(&w, Mode::Frontend, old);
    w.recycle(101);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(w.count("kill"), 0, "pid 101 is someone else now");
    assert_eq!(w.count("terminate"), 0);
    assert!(w.alive(101));
}

#[test]
fn a_recycled_old_pid_is_not_adopted_resumed_or_signalled() {
    let (w, old) = draining_world();
    crash_when(&w, |i| i.phase == Phase::Draining);
    run_to_crash(&w, Mode::Frontend, old);
    w.recycle(100);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "nothing of ours is left to drain");
    assert_eq!(w.count("terminate"), 0, "pid 100 is someone else now");
    assert_eq!(w.count("admin:resume A"), 0);
}

#[test]
fn a_restart_in_a_terminal_phase_does_nothing() {
    let (w, old) = World::with_old();
    assert_eq!(
        Swap::begin(w.ports(), plan(Mode::Frontend), Some(old))
            .unwrap()
            .run(),
        Ok(Phase::Done)
    );
    let n = w.log().len();
    assert_eq!(restart(&w).0, Ok(Phase::Done));
    assert_eq!(w.log().len(), n + 1, "only the restart marker");
}

fn before_in(w: &World, a: &str, b: &str) {
    let log = w.log();
    let i = log
        .iter()
        .position(|l| l.starts_with(a))
        .unwrap_or_else(|| panic!("{a}: {log:#?}"));
    assert!(
        log[i..].iter().any(|l| l.starts_with(b)),
        "`{b}` after `{a}`: {log:#?}"
    );
}
