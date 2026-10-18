//! A proxy restart after the switch where the recorded-active child (the new one) is dead
//! or does not hold the generation. Retire never signals the active slot, so before this
//! change the old child was never retired and the drain waited for ever. Now the restart
//! reconciles first: a failed switch rolls back (the intent is recorded before either
//! child is touched), then retire runs on the dead or non-holder child. Each test names
//! the defect it catches; the mutants planted are listed in the PR.

use super::fakes::*;
use super::model::*;
use super::swap::Swap;

fn crash_when(w: &World, f: impl Fn(&Intent) -> bool + 'static) {
    *w.crash_on_record.borrow_mut() = Some(Box::new(f));
}

fn run_to_crash(w: &World, old: ProcIdent) {
    let r = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old)).and_then(|mut s| s.run());
    assert!(
        matches!(r, Err(Halt::Crashed(_))),
        "expected a crash: {r:?}"
    );
}

fn restart(w: &World) -> (Result<Phase, Halt>, Vec<String>) {
    w.proxy_restart();
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    let end = swap.run();
    (end, swap.notes().to_vec())
}

/// The proxy died while it drained the old child (slot A, pid 100); the new child is pid 101.
fn draining() -> World {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Draining);
    run_to_crash(&w, old);
    w
}

/// The old child serves again behind the proxy's back and the new one stood down.
fn old_holds_new_does_not(w: &World) {
    let mut st = w.st.borrow_mut();
    st.procs[0].holder = true;
    st.procs[0].state = RoleState::Active;
    st.procs[1].holder = false;
    st.procs[1].state = RoleState::StoodDown;
}

/// How many log lines starting with `prefix` came after the last restart.
fn since_restart(w: &World, prefix: &str) -> usize {
    let log = w.log();
    let at = log
        .iter()
        .rposition(|l| l == "proxy restarted")
        .expect("restarted");
    log[at..].iter().filter(|l| l.starts_with(prefix)).count()
}

fn failure_step(w: &World) -> Option<Step> {
    w.last_intent().failure.map(|f| f.step)
}

#[test]
fn a_dead_active_child_and_a_live_old_one_roll_back_and_the_update_fails_at_switch() {
    // Defect: the old child was never retired and the drain waited for ever (the update
    // ended only when the proxy was stopped).
    let w = draining();
    w.kill_pid(101);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.count("admin:resume A"), 1, "the old child serves again");
    assert_eq!(w.holders(), vec![Slot::A]);
    assert!(w.alive(100), "the old child lives and serves");
    assert_eq!(w.count("terminate"), 0, "the dead child needs no signal");
    assert_eq!(w.count("kill"), 0);
    assert_eq!(w.count("spawn"), 1, "nothing was started again");
    assert_eq!(w.count("order revert"), 1);
    assert_eq!(w.count("order confirm"), 1, "only the first run's confirm");
    assert_eq!(w.last_intent().retire, Some(Slot::B));
}

#[test]
fn a_dead_active_child_in_the_bake_rolls_back_without_waiting_for_the_operator() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Baking);
    run_to_crash(&w, old);
    w.kill_pid(101);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(
        w.count("order confirm"),
        0,
        "a failed switch is never confirmed"
    );
}

#[test]
fn an_active_child_that_does_not_hold_the_generation_is_retired_by_sigterm_after_it_drains() {
    // Defect: the non-holder was left running (and the drain never started), or it was
    // SIGKILLed although it served, or SIGTERMed before its streams were gone.
    let w = draining();
    old_holds_new_does_not(&w);
    w.busy(Slot::B, 2, Some(secs(900)));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.count("kill"), 0, "a child that served is never SIGKILLed");
    assert_eq!(w.count("terminate pid 101"), 1);
    assert_eq!(
        w.count("terminate pid 100"),
        0,
        "the active child is never signalled"
    );
    assert!(w.when("terminate pid 101").unwrap() >= secs(900));
    assert_eq!(w.holders(), vec![Slot::A]);
    assert!(w.alive(100) && !w.alive(101));
}

#[test]
fn a_new_child_that_stood_down_and_an_old_one_that_stood_down_too_are_reconciled_by_a_resume() {
    // Neither holds the generation: the old child takes it back (the rule for a cold
    // start is the child's), the new one is retired.
    let w = draining();
    {
        let mut st = w.st.borrow_mut();
        st.procs[1].holder = false;
        st.procs[1].state = RoleState::StoodDown;
        st.procs[0].holder = false;
    }
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(w.count("terminate pid 101"), 1);
}

#[test]
fn the_rollback_is_recorded_before_either_child_is_touched() {
    // Defect: the old child resumed before the intent was on disk, so a crash between
    // the two left a resumed old child and a record that still said the new one serves.
    let w = draining();
    w.kill_pid(101);
    crash_when(&w, |i| i.rolling_back && i.failure.is_some());
    w.proxy_restart_keep_crash_hook();
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    assert!(matches!(swap.run(), Err(Halt::Crashed(_))));
    assert_eq!(
        since_restart(&w, "admin:resume"),
        0,
        "nothing moved before the record"
    );
    assert_eq!(since_restart(&w, "admin:stand_down"), 0);
    let at = w.last_intent();
    assert!(at.rolling_back);
    assert_eq!(at.failure.map(|f| f.step), Some(Step::Switch));
    assert_eq!(
        at.retire, None,
        "the retire slot is decided by the rollback"
    );
}

#[test]
fn a_crash_at_the_rollback_intent_and_a_second_restart_finish_the_rollback_once() {
    let w = draining();
    w.kill_pid(101);
    crash_when(&w, |i| i.rolling_back && i.failure.is_some());
    w.proxy_restart_keep_crash_hook();
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    assert!(matches!(swap.run(), Err(Halt::Crashed(_))));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.count("admin:resume A"), 1);
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(w.count("order revert"), 1);
    // A third restart in the terminal phase changes nothing.
    let n = w.log().len();
    assert_eq!(restart(&w).0, Ok(Phase::Failed));
    assert_eq!(w.log().len(), n + 1);
}

#[test]
fn a_crash_after_the_old_child_resumed_in_a_reconcile_finishes_it_on_the_next_restart() {
    let w = draining();
    old_holds_new_does_not(&w);
    w.crash_at.set(None);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    // Now the same, with the proxy dying inside the rollback's own crash point.
    let w = draining();
    w.kill_pid(101);
    w.proxy_restart();
    w.crash_at.set(Some(CrashPoint::RollbackAfterResume));
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    assert!(matches!(swap.run(), Err(Halt::Crashed(_))));
    assert_eq!(w.count("admin:resume A"), 1);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(w.count("kill"), 0);
    assert_eq!(
        w.count("admin:resume A"),
        2,
        "the resume is repeated: it is idempotent"
    );
}

#[test]
fn both_children_dead_after_the_switch_start_the_new_release_again_and_it_takes_the_generation() {
    // Defect: nobody serves and nobody starts anything. The cold start goes through the
    // spawn port, is recorded, and is the only process started while one lives: a
    // mutant that starts fresh while the old child is alive spawns in the test above.
    let w = draining();
    w.kill_pid(100);
    w.kill_pid(101);
    let (end, notes) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(
        w.count("spawn B"),
        2,
        "the new release starts again in its slot"
    );
    assert_eq!(w.count("spawn A"), 0);
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(
        w.last_intent().new_proc.map(|p| p.pid),
        Some(102),
        "the new process is recorded"
    );
    assert!(w.alive(102));
    assert_eq!(w.count("terminate"), 0);
    assert!(
        notes.iter().any(|n| n.contains("started again")),
        "{notes:?}"
    );
}

#[test]
fn a_cold_start_that_does_not_come_up_ends_the_update_failed_at_restart() {
    for how in ["spawn", "exit", "never ready", "takeover"] {
        let w = draining();
        w.kill_pid(100);
        w.kill_pid(101);
        match how {
            "spawn" => w.st.borrow_mut().spawn_fail_nth.push((Slot::B, 2)),
            "exit" => w.script(Script {
                ready_after: None,
                exit_after: Some(secs(1)),
            }),
            "never ready" => w.script(Script {
                ready_after: None,
                exit_after: None,
            }),
            _ => w.st.borrow_mut().takeover = Takeover::Err,
        }
        let (end, _) = restart(&w);
        assert_eq!(end, Ok(Phase::Failed), "{how}: {:#?}", w.log());
        assert_eq!(failure_step(&w), Some(Step::Restart), "{how}");
    }
}

#[test]
fn a_cold_started_child_that_did_not_take_the_generation_yet_takes_it_after_a_restart() {
    // The proxy died after it spawned the process and before the takeover: only a live,
    // non-holder new child remains. It takes the generation; it is not rolled back to a
    // dead old child.
    let w = draining();
    w.kill_pid(100);
    {
        let mut st = w.st.borrow_mut();
        st.procs[1].holder = false;
        st.procs[1].state = RoleState::Candidate;
    }
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.count("spawn"), 1, "no second process");
}

#[test]
fn a_takeover_that_fails_for_the_only_live_child_ends_the_update_failed() {
    let w = draining();
    w.kill_pid(100);
    {
        let mut st = w.st.borrow_mut();
        st.procs[1].holder = false;
        st.procs[1].state = RoleState::Candidate;
        st.takeover = Takeover::Err;
    }
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure_step(&w), Some(Step::Restart));
}

#[test]
fn both_children_dead_before_the_switch_start_the_old_release_again() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Ready);
    run_to_crash(&w, old);
    w.kill_pid(100);
    w.kill_pid(101);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Restart));
    assert_eq!(
        w.count("spawn A"),
        1,
        "the old release is the recorded-active one"
    );
    assert_eq!(w.count("spawn B"), 1);
    assert_eq!(w.holders(), vec![Slot::A]);
    assert_eq!(w.count("order revert"), 1, "the staged release is dropped");
}

#[test]
fn both_children_dead_before_the_switch_and_the_old_release_cannot_start_fails_at_restart() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Ready);
    run_to_crash(&w, old);
    w.kill_pid(100);
    w.kill_pid(101);
    w.st.borrow_mut().spawn_fail_nth.push((Slot::A, 1));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure_step(&w), Some(Step::Restart));
}

#[test]
fn a_live_active_child_that_does_not_answer_is_not_reconciled_and_the_active_slot_stays_unsignalled()
 {
    // Nothing proves it lost the generation (it is alive, only silent), so the old child,
    // which is the slot this proxy has as active, is neither resumed nor retired.
    let w = draining();
    w.st.borrow_mut().role_fails.push(Slot::B);
    w.set_limit(secs(7200));
    let (end, _) = restart(&w);
    assert_eq!(end, Err(Halt::Interrupted));
    assert!(!w.last_intent().rolling_back);
    assert_eq!(since_restart(&w, "admin:resume"), 0);
    assert_eq!(w.count("terminate"), 0);
    assert_eq!(w.count("kill"), 0);
}

#[test]
fn a_live_active_child_that_does_not_answer_beside_a_dead_old_one_is_left_serving() {
    let w = draining();
    w.st.borrow_mut().role_fails.push(Slot::B);
    w.kill_pid(100);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.count("spawn"), 1);
    assert_eq!(since_restart(&w, "admin:takeover"), 0);
}

#[test]
fn a_rollback_the_old_child_cannot_take_with_a_dead_new_child_fails_at_rollback() {
    // The old child refuses to resume and the new one is dead: there is nobody to put
    // back (no resume is sent to a dead process) and the update ends failed at `rollback`.
    let w = draining();
    w.kill_pid(101);
    w.st.borrow_mut().resume_fails.push(Slot::A);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Rollback));
    assert_eq!(w.count("admin:resume B"), 0);
    assert_eq!(w.count("terminate"), 0);
}
