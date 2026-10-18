//! The failure arms of the swap: a port that refuses, a child that is gone, a restart in
//! the middle of a rollback or a drain. Each test drives one arm through the fakes of
//! [`super::fakes`] and names the defect it catches; the mutants planted to see each one
//! red are listed in the PR.

use std::time::Duration;

use super::fakes::*;
use super::model::*;
use super::swap::Swap;
use crate::testkit::{A, B, c};

fn run(w: &World, p: Plan, old: ProcIdent) -> (Result<Phase, Halt>, Vec<String>) {
    let mut s = Swap::begin(w.ports(), p, Some(old)).expect("begin");
    let end = s.run();
    (end, s.notes().to_vec())
}

fn frontend(w: &World, old: ProcIdent) -> (Result<Phase, Halt>, Vec<String>) {
    run(w, plan(Mode::Frontend), old)
}

fn aio(w: &World, old: ProcIdent) -> (Result<Phase, Halt>, Vec<String>) {
    run(w, plan(Mode::AllInOne), old)
}

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

fn noted(notes: &[String], text: &str) -> bool {
    notes.iter().any(|n| n.contains(text))
}

fn failure(w: &World) -> (Step, String) {
    let f = w.last_intent().failure.expect("the update failed");
    (f.step, f.reason)
}

fn crash_when(w: &World, f: impl Fn(&Intent) -> bool + 'static) {
    *w.crash_on_record.borrow_mut() = Some(Box::new(f));
}

fn run_to_crash(w: &World, p: Plan, old: Option<ProcIdent>) {
    let r = Swap::begin(w.ports(), p, old).and_then(|mut s| s.run());
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

fn never_ready() -> Script {
    Script {
        ready_after: None,
        exit_after: None,
    }
}

fn ready_at(ready: Duration, exit: Option<Duration>) -> Script {
    Script {
        ready_after: Some(ready),
        exit_after: exit,
    }
}

// ---- slots ----

#[test]
fn a_swap_whose_old_child_is_in_slot_b_starts_the_new_one_in_slot_a() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let w = World::new();
        let old = w.install(Slot::B, c(A));
        let mut p = plan(mode);
        p.old_slot = Slot::B;
        let (end, _) = run(&w, p, old);
        assert_eq!(end, Ok(Phase::Done), "{mode:?}");
        assert_eq!(w.count("spawn A"), 1, "{mode:?}: {:#?}", w.log());
        assert_eq!(w.count("spawn B"), 0, "{mode:?}");
        assert_eq!(w.count("terminate pid 100"), 1, "{mode:?}");
        assert_eq!(w.st.borrow().active, Slot::A, "{mode:?}");
    }
}

#[test]
fn a_swap_starts_in_staged_and_shows_its_intent() {
    let (w, old) = World::with_old();
    let s = Swap::begin(w.ports(), plan(Mode::Frontend), Some(old.clone())).expect("begin");
    assert_eq!(s.intent().phase, Phase::Staged);
    assert_eq!(s.intent().old_proc, Some(old));
}

// ---- preflight ----

#[test]
fn an_unreadable_status_fails_the_update_at_stage_before_anything_starts() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let (w, old) = World::with_old();
        w.st.borrow_mut().staged_err = true;
        let (end, _) = run(&w, plan(mode), old);
        assert_eq!(end, Ok(Phase::Failed), "{mode:?}");
        let (step, reason) = failure(&w);
        assert_eq!(step, Step::Stage);
        assert!(reason.contains("status.json"), "{reason}");
        assert_eq!(w.count("spawn") + w.count("migrate") + w.count("hold"), 0);
    }
}

#[test]
fn an_abort_before_the_candidate_starts_fails_the_update_at_abort() {
    let (w, old) = World::with_old();
    w.request_at(Duration::ZERO, Request::Abort);
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Abort);
    assert!(reason.contains("before the candidate started"), "{reason}");
    assert_eq!(w.count("spawn") + w.count("migrate"), 0);
    assert_eq!(w.count("order revert"), 1, "the staged release is dropped");
}

#[test]
fn a_failed_revert_of_the_staged_release_is_noted_and_the_failure_stands() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().migrate_err = Some("lock timeout".into());
    w.st.borrow_mut().revert_err = true;
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure(&w).0, Step::Migrate);
    assert!(
        noted(&notes, "revert of the staged release failed"),
        "{notes:?}"
    );
}

// ---- starting the candidate ----

#[test]
fn a_candidate_that_cannot_be_spawned_fails_the_update_at_start() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().spawn_fail_nth = vec![(Slot::B, 1)];
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Start);
    assert!(reason.starts_with("spawn:"), "{reason}");
    assert_eq!(w.count("hold"), 0, "the old child was never held");
    assert_eq!(w.count("kill"), 0, "there is no candidate to kill");
}

#[test]
fn a_candidate_that_exits_early_and_cannot_be_respawned_fails_the_update_at_start() {
    let (w, old) = World::with_old();
    w.script(ready_at(secs(100), Some(secs(1))));
    w.st.borrow_mut().spawn_fail_nth = vec![(Slot::B, 2)];
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Start);
    assert!(reason.starts_with("respawn:"), "{reason}");
}

#[test]
fn a_candidate_that_never_comes_up_is_killed_and_a_failed_kill_is_noted() {
    let (w, old) = World::with_old();
    w.script(never_ready());
    w.st.borrow_mut().kill_err = true;
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure(&w).0, Step::Ready);
    assert_eq!(w.count("kill pid 101"), 1);
    assert!(noted(&notes, "killing the candidate failed"), "{notes:?}");
}

// ---- the switch ----

#[test]
fn an_abort_that_arrives_after_the_candidate_is_ready_stops_before_the_hold() {
    let (w, old) = World::with_old();
    // Ready at once, so the abort poll inside the wait never runs; the first poll (the
    // preflight's) finds nothing and the switch's finds the abort.
    w.script(ready_at(Duration::ZERO, None));
    w.st.borrow_mut().quiet_polls = 1;
    w.request_at(Duration::ZERO, Request::Abort);
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Abort);
    assert!(reason.contains("before the hold"), "{reason}");
    assert_eq!(w.count("hold"), 0);
    assert_eq!(w.count("kill pid 101"), 1, "the candidate never served");
    assert!(w.alive(100) && !w.alive(101));
}

#[test]
fn an_old_child_that_is_gone_fails_the_switch_at_quiesce_and_the_failed_resume_is_noted() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let (w, old) = World::with_old();
        w.kill_pid(100);
        w.hold_streams(4);
        let (end, notes) = run(&w, plan(mode), old);
        assert_eq!(end, Ok(Phase::Failed), "{mode:?}");
        let (step, reason) = failure(&w);
        assert_eq!(step, Step::Switch, "{mode:?}");
        assert!(reason.starts_with("quiesce:"), "{reason}");
        assert!(noted(&notes, "resume of A failed"), "{mode:?}: {notes:?}");
        assert_eq!(w.count("admin:resume A"), 1, "{mode:?}");
        assert_eq!(
            w.count("release_into A 4"),
            1,
            "{mode:?}: the streams go back"
        );
    }
}

#[test]
fn a_quiesce_that_lands_after_its_budget_fails_the_switch_but_one_at_the_budget_passes() {
    // The fake quiesces 200 ms after the call; the wait looks every 100 ms.
    let (w, old) = World::with_old();
    let mut late = plan(Mode::Frontend);
    late.timing.quiesce = Duration::from_millis(150);
    let (end, _) = run(&w, late, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Switch);
    assert_eq!(reason, "quiesce took 200 ms");
    assert_eq!(w.count("admin:takeover"), 0);

    let (w, old) = World::with_old();
    let mut exact = plan(Mode::Frontend);
    exact.timing.quiesce = Duration::from_millis(200);
    let (end, _) = run(&w, exact, old);
    assert_eq!(end, Ok(Phase::Done), "quiesced exactly at the budget");
}

#[test]
fn a_takeover_onto_a_candidate_that_died_fails_the_switch_and_resumes_the_old_child() {
    let (w, old) = World::with_old();
    // Ready at 1 s, gone at 1.05 s: it dies while the old child quiesces.
    w.script(ready_at(secs(1), Some(Duration::from_millis(1050))));
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Switch);
    assert!(reason.starts_with("takeover:"), "{reason}");
    assert_eq!(w.state_of(100), RoleState::Active);
    assert_eq!(w.holders(), vec![Slot::A]);
}

#[test]
fn a_refused_resume_after_a_failed_takeover_is_noted() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().takeover = Takeover::Err;
    w.st.borrow_mut().resume_fails = vec![Slot::A];
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure(&w).0, Step::Switch);
    assert!(noted(&notes, "resume of A failed"), "{notes:?}");
}

#[test]
fn a_held_stream_release_that_fails_halts_the_swap() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().release_err = true;
    let (end, _) = frontend(&w, old);
    assert!(
        matches!(&end, Err(Halt::Port(m)) if m.contains("release into B")),
        "{end:?}"
    );
}

#[test]
fn a_refused_stand_down_after_the_switch_is_noted_and_the_update_goes_on() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().stand_down_err = true;
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(
        noted(&notes, "stand-down of the old child failed"),
        "{notes:?}"
    );
}

// ---- the bake ----

#[test]
fn an_abort_during_the_bake_is_refused_and_the_update_finishes() {
    let (w, old) = World::with_old();
    w.request_at(secs(10), Request::Abort);
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(
        noted(&notes, "abort refused: the swap already switched"),
        "{notes:?}"
    );
    assert_eq!(w.count("admin:resume"), 0);
}

#[test]
fn a_new_child_that_dies_in_the_bake_keeps_it_red_and_the_confirm_is_never_sent() {
    let (w, old) = World::with_old();
    // The candidate is ready at 2 s and exits at 100 s, inside the 300 s bake.
    w.script(ready_at(secs(2), Some(secs(100))));
    w.set_limit(secs(2000));
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Err(Halt::Interrupted), "the bake never turns green");
    assert_eq!(w.count("order confirm"), 0);
}

#[test]
fn a_baking_update_with_no_recorded_candidate_stays_red_and_never_confirms() {
    let (w, old) = World::with_old();
    let p = plan(Mode::Frontend);
    let mut intent = Intent::begin(&p, Some(old), 0);
    intent.phase = Phase::Baking;
    let mut s = Swap::resumed(w.ports(), p, intent, Slot::B, true, false);
    w.set_limit(secs(2000));
    assert_eq!(s.run(), Err(Halt::Interrupted));
    assert_eq!(w.count("order confirm"), 0);
}

#[test]
fn a_rollback_asked_for_when_the_old_child_has_exited_is_refused_and_the_bake_goes_on() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().procs[0].script.exit_after = Some(secs(20));
    w.request_at(secs(30), Request::Rollback);
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(
        noted(&notes, "rollback refused: the old child is not running"),
        "{notes:?}"
    );
    assert_eq!(w.count("admin:resume"), 0);
    assert!(!w.last_intent().rolling_back);
}

#[test]
fn a_rollback_asked_for_on_an_all_in_one_host_is_refused() {
    let (w, old) = World::with_old();
    w.request_at(secs(10), Request::Rollback);
    let (end, notes) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(
        noted(&notes, "rollback refused: the old child is not running"),
        "{notes:?}"
    );
    assert_eq!(w.count("admin:resume"), 0);
}

// ---- the rollback ----

fn rollback_world() -> (World, ProcIdent) {
    let (w, old) = World::with_old();
    w.request_at(secs(60), Request::Rollback);
    (w, old)
}

#[test]
fn a_rollback_whose_old_child_never_takes_the_generation_back_fails_and_leaves_the_new_child_serving()
 {
    let (w, old) = rollback_world();
    // The new child cannot be told to stand down, so it keeps the generation and the old
    // child's resume cannot take it.
    w.st.borrow_mut().stand_down_err = true;
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Rollback);
    assert!(
        reason.contains("did not take the generation back"),
        "{reason}"
    );
    assert!(
        noted(&notes, "stand-down of the new child failed"),
        "{notes:?}"
    );
    assert_eq!(
        w.count("admin:resume B"),
        0,
        "the new child never stopped serving"
    );
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.count("order revert"), 0, "the staged release stays");
    assert!(!w.last_intent().rolling_back);
}

#[test]
fn a_rollback_that_cannot_resume_either_child_notes_it_and_fails_at_rollback() {
    let (w, old) = rollback_world();
    w.st.borrow_mut().resume_fails = vec![Slot::A, Slot::B];
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure(&w).0, Step::Rollback);
    assert!(noted(&notes, "resume of the new child failed"), "{notes:?}");
    assert_eq!(
        w.count("admin:resume B"),
        1,
        "the stood-down new child is put back"
    );
}

// ---- retire ----

#[test]
fn a_drain_with_no_recorded_old_process_ends_the_update_without_a_signal() {
    let (w, _old) = World::with_old();
    let p = plan(Mode::Frontend);
    let mut intent = Intent::begin(&p, None, 0);
    intent.phase = Phase::Draining;
    let mut s = Swap::resumed(w.ports(), p, intent, Slot::B, true, false);
    assert_eq!(s.run(), Ok(Phase::Done));
    assert_eq!(w.count("terminate") + w.count("kill"), 0);
}

#[test]
fn a_failed_sigterm_is_noted_and_the_exit_is_still_awaited() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().terminate_err = true;
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(noted(&notes, "SIGTERM to A failed"), "{notes:?}");
}

#[test]
fn a_child_that_takes_five_seconds_to_stop_is_told_once() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().term_takes = secs(5);
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert_eq!(w.count("terminate pid 100"), 1, "one SIGTERM, then wait");
}

#[test]
fn a_failure_that_is_not_the_confirm_does_not_end_a_finished_drain_as_failed() {
    let (w, old) = World::with_old();
    let p = plan(Mode::Frontend);
    let mut intent = Intent::begin(&p, Some(old), 0);
    intent.phase = Phase::Draining;
    intent.failure = Some(Failure {
        step: Step::Start,
        reason: "an earlier note".into(),
    });
    let mut s = Swap::resumed(w.ports(), p, intent, Slot::B, true, false);
    assert_eq!(s.run(), Ok(Phase::Done));
}

#[test]
fn the_slot_that_may_still_be_the_active_one_is_never_signalled() {
    // The new child is alive and holds the generation, so it is the active slot: it is
    // never signalled, the old child (not the active one) is retired, and no rollback runs.
    // (With the new child dead the old child used to be left alone for ever; the restart
    // now reconciles that case first: `reconcile_tests`.)
    let (w, old) = World::with_old();
    w.crash_at.set(Some(CrashPoint::WhileDraining));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done));
    assert_eq!(w.count("terminate pid 101"), 0);
    assert_eq!(w.count("terminate pid 100"), 1);
    assert_eq!(w.count("admin:resume"), 0);
    assert!(w.alive(101));
}

// ---- restarts ----

#[test]
fn a_restart_in_a_finished_all_in_one_update_changes_nothing() {
    let (w, old) = World::with_old();
    assert_eq!(aio(&w, old).0, Ok(Phase::Done));
    let seen = w.log().len();
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done));
    assert_eq!(w.log().len(), seen + 1, "only `proxy restarted` was logged");
}

#[test]
fn a_restart_after_the_all_in_one_switch_releases_the_hold_into_the_live_new_process() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Switched);
    run_to_crash(&w, plan(Mode::AllInOne), Some(old));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done));
    seq(&w, &["proxy restarted", "release_into B"]);
}

#[test]
fn a_restart_after_the_all_in_one_switch_with_a_dead_new_process_releases_nothing() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Switched);
    run_to_crash(&w, plan(Mode::AllInOne), Some(old));
    w.kill_pid(101);
    w.set_limit(secs(3600));
    let (end, _) = restart(&w);
    assert_eq!(end, Err(Halt::Interrupted), "the bake stays red");
    let after = w.pos("proxy restarted").expect("restarted");
    assert!(
        w.log()[after..]
            .iter()
            .all(|l| !l.starts_with("release_into")),
        "{:#?}",
        w.log()
    );
}

#[test]
fn a_restart_after_the_all_in_one_candidate_was_recorded_waits_for_it_instead_of_spawning_again() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Switching && i.new_proc.is_some());
    run_to_crash(&w, plan(Mode::AllInOne), Some(old));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.count("spawn B"), 1);
    assert_eq!(w.count("migrate"), 1);
    seq(&w, &["proxy restarted", "release_into B"]);
}

#[test]
fn a_restart_after_the_all_in_one_candidate_was_recorded_restarts_the_old_release_if_it_is_not_ready()
 {
    let (w, old) = World::with_old();
    w.script(never_ready());
    crash_when(&w, |i| i.phase == Phase::Switching && i.new_proc.is_some());
    run_to_crash(&w, plan(Mode::AllInOne), Some(old));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure(&w).0, Step::Ready);
    seq(
        &w,
        &[
            "proxy restarted",
            "kill pid 101",
            "spawn A",
            "release_into A",
        ],
    );
}

#[test]
fn a_restart_while_the_all_in_one_old_process_quiesced_and_died_goes_forward() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(10_000);
    crash_when(&w, |i| i.phase == Phase::Quiescing);
    run_to_crash(&w, plan(Mode::AllInOne), Some(old));
    w.kill_pid(100);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.count("admin:resume"), 0, "there is nothing to resume");
    assert_eq!(w.count("terminate"), 0, "there is nothing to stop");
    seq(&w, &["proxy restarted", "migrate", "spawn B"]);
}

#[test]
fn a_candidate_recorded_alive_with_no_process_is_a_halt_not_a_panic() {
    let (w, old) = World::with_old();
    let p = plan(Mode::AllInOne);
    let mut intent = Intent::begin(&p, Some(old), 0);
    intent.phase = Phase::Switching;
    let mut s = Swap::resumed(w.ports(), p, intent, Slot::A, false, true);
    let end = s.run();
    assert!(
        matches!(&end, Err(Halt::Port(m)) if m.contains("no candidate process")),
        "{end:?}"
    );
}

#[test]
fn a_kill_of_a_candidate_with_no_process_signals_nothing() {
    // Alive but not recorded, before the switch, with the old child gone is not a state the
    // swap makes; a restart that finds it must not signal a pid it does not hold.
    let (w, old) = World::with_old();
    let p = plan(Mode::Frontend);
    let mut intent = Intent::begin(&p, Some(old), 0);
    intent.phase = Phase::Starting;
    let mut s = Swap::resumed(w.ports(), p, intent, Slot::A, true, true);
    assert_eq!(s.run(), Ok(Phase::Failed));
    assert_eq!(w.count("kill"), 0);
    assert_eq!(failure(&w).0, Step::Restart);
}

#[test]
fn an_all_in_one_update_with_no_recorded_old_process_halts_rather_than_guessing() {
    let (w, _old) = World::with_old();
    let mut s = Swap::begin(w.ports(), plan(Mode::AllInOne), None).expect("begin");
    let end = s.run();
    assert!(
        matches!(&end, Err(Halt::Port(m)) if m.contains("no old process is recorded")),
        "{end:?}"
    );
    assert_eq!(w.count("terminate"), 0);
}

#[test]
fn an_all_in_one_hold_that_is_refused_fails_the_update_before_the_old_process_is_touched() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().hold_fails_from = Some(1);
    let (end, _) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Hold);
    assert!(reason.contains("hold refused"), "{reason}");
    assert_eq!(w.count("admin:quiesce") + w.count("terminate"), 0);
}

#[test]
fn an_all_in_one_old_process_that_exits_by_itself_while_quiescing_is_not_signalled() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(10_000);
    w.st.borrow_mut().procs[0].script.exit_after = Some(secs(5));
    let (end, _) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert_eq!(w.count("terminate"), 0);
    seq(&w, &["hold", "migrate", "spawn B", "release_into B"]);
}

#[test]
fn a_refused_sigterm_to_the_all_in_one_old_process_is_noted_and_the_exit_awaited() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().terminate_err = true;
    let (end, notes) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(
        noted(&notes, "SIGTERM to the old process failed"),
        "{notes:?}"
    );
}

#[test]
fn an_all_in_one_new_process_that_cannot_be_spawned_brings_the_old_release_back() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().spawn_fail_nth = vec![(Slot::B, 1)];
    w.hold_streams(3);
    let (end, _) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Start);
    assert!(reason.starts_with("spawn:"), "{reason}");
    seq(
        &w,
        &[
            "terminate pid 100",
            "spawn B refused",
            "spawn A",
            "release_into A 3",
        ],
    );
}

#[test]
fn an_old_release_that_does_not_come_back_is_given_ready_within_and_noted() {
    // Not ready for ever: gives up at 120 s. Exits at 1 s: gives up at the exit.
    for (script, long) in [
        (never_ready(), true),
        (ready_at(secs(500), Some(secs(1))), false),
    ] {
        let (w, old) = World::with_old();
        w.st.borrow_mut().migrate_err = Some("lock timeout".into());
        w.script(script);
        w.hold_streams(5);
        let (end, notes) = aio(&w, old);
        assert_eq!(end, Ok(Phase::Failed));
        assert_eq!(failure(&w).0, Step::Migrate);
        assert!(
            noted(&notes, "the old release did not come back"),
            "{notes:?}"
        );
        assert_eq!(w.count("release_into"), 0, "nothing serves, nothing is fed");
        if long {
            assert!(w.elapsed() >= secs(120), "gave up after {:?}", w.elapsed());
        } else {
            assert!(
                w.elapsed() < secs(60),
                "waited on after the exit: {:?}",
                w.elapsed()
            );
        }
    }
}

#[test]
fn an_old_release_that_cannot_even_be_spawned_again_is_noted() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().migrate_err = Some("lock timeout".into());
    w.st.borrow_mut().spawn_fail_nth = vec![(Slot::A, 1)];
    let (end, notes) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(failure(&w).0, Step::Migrate);
    assert!(
        noted(&notes, "restarting the old release failed"),
        "{notes:?}"
    );
}

// ---- restarts inside a rollback or a switch ----

#[test]
fn a_restart_while_a_rollback_drains_the_new_child_finishes_the_rollback() {
    let (w, old) = rollback_world();
    w.crash_at.set(Some(CrashPoint::WhileDraining));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    assert!(w.last_intent().rolling_back);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::RolledBack), "{:#?}", w.log());
    assert_eq!(w.count("terminate pid 101"), 1);
    assert_eq!(w.count("kill"), 0);
    assert!(w.alive(100) && !w.alive(101));
    // Past the bake the rollback is not run again: no second hold, resume or revert.
    assert_eq!(w.count("hold"), 2, "the switch's and the rollback's");
    assert_eq!(w.count("admin:resume A"), 1);
    assert_eq!(w.count("order revert"), 1);
}

#[test]
fn a_restart_while_the_old_child_drains_when_the_new_one_lost_the_generation_rolls_back_first() {
    // The new child is alive and the store says it is not the holder. The old child must
    // not be left waiting on a slot that serves nothing: the restart rolls back (hold,
    // resume the old child), retires the new child by SIGTERM and ends the update `failed`
    // at `switch`. It used to leave both children running for ever.
    let (w, old) = World::with_old();
    w.crash_at.set(Some(CrashPoint::WhileDraining));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    w.st.borrow_mut().procs[1].holder = false;
    w.set_limit(secs(7200));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure(&w).0, Step::Switch);
    assert_eq!(w.count("hold"), 2, "the switch's and the rollback's");
    assert_eq!(w.count("admin:resume A"), 1);
    assert_eq!(w.count("terminate pid 101"), 1);
    assert_eq!(w.count("terminate pid 100"), 0);
    assert_eq!(w.count("kill"), 0);
}

#[test]
fn a_restart_in_a_rollback_whose_old_child_died_fails_at_restart_and_ends_the_rollback() {
    let (w, old) = rollback_world();
    w.crash_at.set(Some(CrashPoint::RollbackAfterResume));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    w.kill_pid(100);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Restart);
    assert!(reason.contains("the old child is gone"), "{reason}");
    assert!(!w.last_intent().rolling_back);
    assert_eq!(w.count("terminate"), 0);
}

#[test]
fn a_restart_in_a_rollback_whose_hold_is_refused_notes_it_and_finishes_the_rollback() {
    let (w, old) = rollback_world();
    w.crash_at.set(Some(CrashPoint::RollbackAfterResume));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    // The swap's hold was the first, the rollback's the second, the restart's the third.
    w.st.borrow_mut().hold_fails_from = Some(3);
    let (end, notes) = restart(&w);
    assert_eq!(end, Ok(Phase::RolledBack), "{:#?}", w.log());
    assert!(noted(&notes, "hold for the rollback failed"), "{notes:?}");
}

#[test]
fn a_restart_after_the_takeover_with_a_refused_stand_down_notes_it_and_completes() {
    let (w, old) = World::with_old();
    w.crash_at.set(Some(CrashPoint::AfterTakeoverBeforeSwap));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    w.st.borrow_mut().stand_down_err = true;
    let (end, notes) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert!(
        noted(&notes, "stand-down of the old child failed"),
        "{notes:?}"
    );
}

#[test]
fn a_restart_before_the_swap_when_the_old_child_is_gone_leaves_the_candidate_alone() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Holding);
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    w.kill_pid(100);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Restart);
    assert!(reason.ends_with("the old child is gone"), "{reason}");
    assert_eq!(
        w.count("kill"),
        0,
        "the candidate may be the only child left"
    );
    assert!(w.alive(101));
}

#[test]
fn a_restart_that_finds_no_old_process_recorded_treats_the_old_child_as_gone() {
    let (w, _old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Holding);
    run_to_crash(&w, plan(Mode::Frontend), None);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed));
    assert!(failure(&w).1.ends_with("the old child is gone"));
    assert_eq!(w.count("admin:resume"), 0, "nothing of ours to resume");
}

// ---- the fakes that no swap path reaches ----

#[test]
fn the_fake_root_records_a_frontend_update_order() {
    // The swap never sends this order (the host side does, before the swap starts); the
    // port is part of the trait the fake implements, so the fake's arm is exercised here.
    let w = World::new();
    let locator = Locator {
        order_id: 7,
        epoch: 1,
        generation: 2,
        verb: Verb::FrontendUpdate,
        commit: c(B),
        window_s: 60,
    };
    w.ports()
        .orders
        .frontend_update(&locator)
        .expect("the fake accepts it");
    assert_eq!(w.count("order frontend_update 7"), 1);
    // And a slot with no live child answers nothing, as the real admin port would not.
    assert!(w.ports().admin.open_counts(Slot::B).is_err());
}

#[test]
fn a_rollback_waits_for_the_old_child_to_report_active_not_only_to_hold_the_generation() {
    let (w, old) = rollback_world();
    w.st.borrow_mut().resume_lag = secs(3);
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::RolledBack), "{:#?}", w.log());
    let resumed = w.when("admin:resume A").expect("resumed");
    let fed = w.when("release_into A").expect("released");
    assert!(
        fed >= resumed + secs(3),
        "the hold was released {fed:?} after the START, 3 s were due after {resumed:?}"
    );
}

#[test]
fn a_rollback_whose_old_child_holds_the_generation_but_never_reports_active_fails() {
    let (w, old) = rollback_world();
    w.st.borrow_mut().resume_lag = secs(3600);
    let (end, _) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    let (step, reason) = failure(&w);
    assert_eq!(step, Step::Rollback);
    assert!(
        reason.contains("did not take the generation back"),
        "{reason}"
    );
    assert_eq!(w.count("order revert"), 0, "the staged release stays");
    assert_eq!(
        w.count("admin:resume B"),
        1,
        "the stood-down new child is put back"
    );
}

#[test]
fn a_new_child_that_is_active_but_does_not_hold_the_generation_is_resumed_not_trusted() {
    let (w, old) = rollback_world();
    w.crash_at.set(Some(CrashPoint::RollbackAfterResume));
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    // The new child came back up as active, but the store gave the generation to the old one.
    w.st.borrow_mut().procs[1].state = RoleState::Active;
    w.st.borrow_mut().stand_down_err = true;
    w.st.borrow_mut().resume_fails = vec![Slot::A];
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure(&w).0, Step::Rollback);
    let after = w.pos("proxy restarted").expect("restarted");
    assert!(
        w.log()[after..].iter().any(|l| l == "admin:resume B"),
        "{:#?}",
        w.log()
    );
}

#[test]
fn an_old_child_that_is_active_but_lost_the_generation_is_resumed_on_a_restart() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Holding);
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    w.st.borrow_mut().procs[0].holder = false;
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure(&w).0, Step::Restart);
    assert_eq!(w.count("admin:resume A"), 1, "it takes the generation back");
    assert_eq!(w.holders(), vec![Slot::A]);
}

#[test]
fn an_old_child_the_proxy_does_not_hold_is_never_resumed() {
    let (w, _old) = World::with_old();
    w.st.borrow_mut().takeover = Takeover::Err;
    let mut s = Swap::begin(w.ports(), plan(Mode::Frontend), None).expect("begin");
    assert_eq!(s.run(), Ok(Phase::Failed));
    assert_eq!(failure(&w).0, Step::Switch);
    assert_eq!(w.count("admin:resume"), 0, "{:#?}", w.log());
}

#[test]
fn a_rollback_asked_for_after_the_new_child_died_notes_the_stand_down_and_completes() {
    let (w, old) = World::with_old();
    w.script(ready_at(secs(2), Some(secs(100))));
    w.request_at(secs(150), Request::Rollback);
    let (end, notes) = frontend(&w, old);
    assert_eq!(end, Ok(Phase::RolledBack), "{:#?}", w.log());
    assert!(
        noted(&notes, "stand-down of the new child failed"),
        "{notes:?}"
    );
    assert_eq!(w.count("terminate"), 0, "there is no new process to stop");
    assert_eq!(w.holders(), vec![Slot::A]);
}

#[test]
fn a_restart_that_finds_the_new_childs_pid_recycled_rolls_back_and_never_signals_it() {
    let (w, old) = World::with_old();
    crash_when(&w, |i| i.phase == Phase::Baking);
    run_to_crash(&w, plan(Mode::Frontend), Some(old));
    // The pid is alive, but it is another process now: the new child is not ours to trust.
    w.recycle(101);
    // The store's holder was the child that died; the pid's new owner holds nothing.
    w.st.borrow_mut().procs[1].holder = false;
    let (end, _) = restart(&w);
    assert_eq!(
        end,
        Ok(Phase::Failed),
        "a dead recorded-active child: the switch failed"
    );
    assert_eq!(failure(&w).0, Step::Switch);
    assert_eq!(w.count("order confirm"), 0);
    assert_eq!(w.count("terminate pid 101") + w.count("kill pid 101"), 0);
    assert!(w.alive(101), "pid 101 is someone else now");
    assert_eq!(w.holders(), vec![Slot::A]);
}
