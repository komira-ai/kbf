//! A proxy restart after the switch where the recorded-active child (the new one) is dead
//! or does not hold the generation. Retire never signals the active slot, so before this
//! change the old child was never retired and the drain waited for ever. Now the restart
//! reconciles first: a failed switch rolls back (the intent is recorded before either
//! child is touched), then retire runs on the dead or non-holder child. Each test names
//! the defect it catches; the mutants planted are listed in the PR.
//!
//! Which side of the confirm the crash is on decides the answer. `baking()` crashes before
//! the root's confirm landed, so a dead or non-holder new child is a failed switch and the
//! update rolls back. `draining()` crashes after it: `current` names the new release, the
//! store wins, and the restart never rolls back.

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

/// The proxy died as the bake began: the switch landed, the confirm did not.
fn baking() -> World {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Baking);
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
fn a_dead_active_child_and_a_live_old_one_before_the_confirm_roll_back_and_the_update_fails_at_switch()
 {
    // Defect: the old child was never retired and the drain waited for ever (the update
    // ended only when the proxy was stopped).
    let w = baking();
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
    assert_eq!(
        w.count("order confirm"),
        0,
        "a failed switch is never confirmed"
    );
    assert_eq!(
        since_restart(&w, "admin:stand_down"),
        0,
        "a dead new child is never sent stand_down"
    );
    assert_eq!(w.last_intent().retire, Some(Slot::B));
}

#[test]
fn an_active_child_that_does_not_hold_the_generation_is_retired_by_sigterm_after_it_drains() {
    // Defect: the non-holder was left running (and the drain never started), or it was
    // SIGKILLed although it served, or SIGTERMed before its streams were gone.
    let w = baking();
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
    let w = baking();
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
    let w = baking();
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
    let w = baking();
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
    let w = baking();
    old_holds_new_does_not(&w);
    w.crash_at.set(None);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    // Now the same, with the proxy dying inside the rollback's own crash point.
    let w = baking();
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
    let w = baking();
    w.kill_pid(101);
    w.st.borrow_mut().resume_fails.push(Slot::A);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Rollback));
    assert_eq!(w.count("admin:resume B"), 0);
    assert_eq!(w.count("terminate"), 0);
}

/// The index of the last log line that starts with `prefix`.
fn last_at(w: &World, prefix: &str) -> usize {
    w.log()
        .iter()
        .rposition(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} in {:#?}", w.log()))
}

#[test]
fn a_cold_started_child_gets_the_traffic_and_the_held_streams() {
    // Defect: the new release came up and took the generation, but the gate was never
    // released into its slot, so the proxy kept routing to the dead one and every held
    // stream waited out `hold_max`.
    let w = draining();
    w.kill_pid(100);
    w.kill_pid(101);
    w.proxy_restart();
    w.hold_streams(3);
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    assert_eq!(swap.run(), Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(swap.active, Slot::B, "the swap routes to the new slot");
    assert_eq!(w.st.borrow().active, Slot::B, "the gate routes to it");
    assert_eq!(w.st.borrow().fed.last(), Some(&(Slot::B, 3)));
    assert_eq!(w.held(), 0);
    assert_eq!(w.count("gate:expired"), 0);
    assert!(
        last_at(&w, "release_into B 3") > last_at(&w, "admin:takeover B"),
        "released after the takeover resolved"
    );
}

#[test]
fn a_crash_after_the_cold_start_was_recorded_is_adopted_and_the_cold_start_repeats_once() {
    // Defect: the process was spawned and not yet recorded when the proxy died again, so
    // the next restart started a second one beside it (two live children, one orphan).
    // The record goes down right after the spawn; the next restart adopts that process.
    let w = draining();
    w.kill_pid(100);
    w.kill_pid(101);
    crash_when(&w, |i| i.new_proc.as_ref().is_some_and(|p| p.pid == 102));
    w.proxy_restart_keep_crash_hook();
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    assert!(matches!(swap.run(), Err(Halt::Crashed(_))));
    assert_eq!(w.count("spawn B"), 2);
    assert_eq!(
        since_restart(&w, "admin:takeover"),
        0,
        "the crash came with the spawn record, before the takeover"
    );
    assert!(w.holders().is_empty());
    assert_eq!(w.last_intent().new_proc.map(|p| p.pid), Some(102));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.count("spawn B"), 2, "the recorded process was adopted");
    assert_eq!(w.holders(), vec![Slot::B]);
    assert!(w.alive(102) && !w.alive(101));
    assert_eq!(w.last_intent().new_proc.map(|p| p.pid), Some(102));
    assert_eq!(w.count("terminate"), 0);
}

#[test]
fn after_the_confirm_a_dead_new_child_is_started_again_and_the_old_one_drains_and_retires() {
    // Defect: the restart resumed the old child although `current` names the new release,
    // so the old release served while the host would boot the new one.
    let w = draining();
    w.kill_pid(101);
    w.busy(Slot::A, 2, Some(secs(900)));
    let (end, notes) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.last_intent().failure, None);
    assert!(!w.last_intent().rolling_back);
    assert_eq!(w.count("spawn B"), 2, "the new release starts again");
    assert_eq!(w.count("spawn A"), 0, "nothing is started for the old slot");
    assert_eq!(w.count("admin:resume"), 0, "the old child is not resumed");
    assert_eq!(w.count("order revert"), 0);
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.st.borrow().active, Slot::B);
    assert_eq!(w.last_intent().new_proc.map(|p| p.pid), Some(102));
    assert_eq!(since_restart(&w, "admin:stand_down A"), 1);
    assert_eq!(w.count("kill"), 0, "a served child is never SIGKILLed");
    assert_eq!(w.count("terminate pid 100"), 1, "the old child is retired");
    assert_eq!(w.count("terminate pid 102"), 0, "the active child is not");
    assert!(w.when("terminate pid 100").unwrap() >= secs(900));
    assert!(last_at(&w, "terminate pid 100") > last_at(&w, "admin:takeover B"));
    assert!(!w.alive(100) && w.alive(102));
    assert!(notes.iter().any(|n| n.contains("started again")));
}

#[test]
fn after_the_confirm_a_new_child_that_does_not_hold_the_generation_takes_over_and_the_old_one_retires()
 {
    // Defect: the old child was resumed (a rollback) although `current` names the new
    // release. Here the old child even holds the generation again behind the proxy's back.
    let w = draining();
    {
        let mut st = w.st.borrow_mut();
        st.procs[0].holder = true;
        st.procs[0].state = RoleState::Active;
        st.procs[1].holder = false;
        st.procs[1].state = RoleState::Candidate;
        st.stand_down_err = true;
    }
    let (end, notes) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.last_intent().failure, None);
    assert_eq!(w.count("admin:resume"), 0);
    assert_eq!(w.count("spawn"), 1, "no second process");
    assert_eq!(since_restart(&w, "admin:takeover B"), 1);
    assert_eq!(since_restart(&w, "admin:stand_down A"), 1);
    assert!(last_at(&w, "admin:stand_down A") < last_at(&w, "admin:takeover B"));
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.st.borrow().active, Slot::B);
    assert_eq!(w.count("kill"), 0);
    assert_eq!(w.count("terminate pid 100"), 1);
    assert_eq!(w.count("terminate pid 101"), 0);
    assert!(
        notes.iter().any(|n| n.contains("stand-down")),
        "a refused stand-down is noted, the fence holds: {notes:?}"
    );
}

#[test]
fn after_the_confirm_a_new_child_that_cannot_take_over_ends_failed_and_the_old_one_is_not_resumed()
{
    let w = draining();
    {
        let mut st = w.st.borrow_mut();
        st.procs[1].holder = false;
        st.procs[1].state = RoleState::Candidate;
        st.takeover = Takeover::Err;
    }
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Restart));
    assert_eq!(w.count("admin:resume"), 0);
    assert_eq!(w.count("order revert"), 0, "the staged release stays");
}

#[test]
fn after_the_confirm_a_new_child_that_cannot_be_started_again_ends_failed_at_restart() {
    let w = draining();
    w.kill_pid(101);
    w.st.borrow_mut().spawn_fail_nth.push((Slot::B, 2));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Restart));
    assert_eq!(w.count("admin:resume"), 0, "no rollback after the confirm");
    assert!(w.alive(100), "the old child is not killed");
}

#[test]
fn a_confirm_that_never_landed_is_not_a_confirm_even_in_the_drain() {
    // The root refused every confirm: `current` still names the old release, so a dead new
    // child is a failed switch and rolls back, although the phase is past `nodes`.
    let (w, old) = World::with_old();
    w.st.borrow_mut().confirm_err = true;
    crash_when(&w, |i| i.phase == Phase::Draining);
    run_to_crash(&w, old);
    assert_eq!(failure_step(&w), Some(Step::Confirm));
    w.kill_pid(101);
    w.st.borrow_mut().confirm_err = false;
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.count("admin:resume A"), 1);
    assert_eq!(
        w.count("spawn B"),
        1,
        "the new release is not started again"
    );
    assert_eq!(w.holders(), vec![Slot::A]);
}

/// Catches a cold start that takes the generation without naming it (or names a stale one):
/// no child holds it, so the candidate's own view of the store is the only source, and the
/// fake store (at 5, not 1) refuses any other number. The release is started waiting.
#[test]
fn a_cold_start_names_the_generation_the_candidate_reads_from_the_store() {
    let w = draining();
    w.kill_pid(100);
    w.kill_pid(101);
    w.st.borrow_mut().generation = 5;
    w.st.borrow_mut().starts.clear();
    w.proxy_restart();
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    assert_eq!(swap.run(), Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.count("admin:takeover B expect 5"), 1, "{:#?}", w.log());
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.st.borrow().starts, vec![(Slot::B, Start::Candidate)]);
}

/// Catches a cold start whose candidate cannot be read: the generation to name comes from
/// its own view, so a view that fails ends the cold start with the reason instead of
/// taking over with a guess. The update fails, and nothing took the generation.
#[test]
fn a_cold_start_whose_candidate_cannot_be_read_fails_without_a_takeover() {
    let w = draining();
    w.kill_pid(100);
    w.kill_pid(101);
    w.st.borrow_mut().role_fails.push(Slot::B);
    w.proxy_restart();
    let intent = w.last_intent();
    let mut swap = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::frontend()),
        intent,
    );
    let end = swap.run();
    assert!(end.is_ok(), "{end:?}");
    assert_eq!(since_restart(&w, "admin:takeover"), 0, "{:#?}", w.log());
    assert!(w.holders().is_empty());
}
