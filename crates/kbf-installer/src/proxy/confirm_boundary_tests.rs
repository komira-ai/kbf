//! Where the confirm boundary lies for a restarted proxy: the root's `current` names the
//! new release (FP sections 6 and 8). `update.json` is written around the confirm, not
//! with it, so the phase cannot be the boundary; the restart asks the root. Each test
//! names the defect it catches.

use super::fakes::*;
use super::model::*;
use super::swap::Swap;
use crate::testkit::{A, B, c};

fn crash_when(w: &World, f: impl Fn(&Intent) -> bool + 'static) {
    *w.crash_on_record.borrow_mut() = Some(Box::new(f));
}

fn run_to_crash(w: &World, old: ProcIdent, nodes: bool) {
    let mut p = plan(Mode::Frontend);
    p.nodes = nodes;
    let r = Swap::begin(w.ports(), p, Some(old)).and_then(|mut s| s.run());
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

/// Log lines starting with `prefix` after the last restart.
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

/// The proxy died in the record of `phase`.
fn crashed_at(phase: Phase, nodes: bool) -> World {
    let (w, old) = World::with_old();
    crash_when(&w, move |i| i.phase == phase);
    run_to_crash(&w, old, nodes);
    w
}

/// The proxy died right after the root's confirm landed, before `update.json` knew.
fn crashed_after_confirm(nodes: bool) -> World {
    let (w, old) = World::with_old();
    w.crash_at.set(Some(CrashPoint::AfterConfirmLanded));
    run_to_crash(&w, old, nodes);
    w
}

fn assert_post_confirm_restart(w: &World, end: Result<Phase, Halt>) {
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.last_intent().failure, None);
    assert!(!w.last_intent().rolling_back);
    assert_eq!(w.count("spawn B"), 2, "the new release starts again");
    assert_eq!(w.count("admin:resume"), 0, "no rollback after the confirm");
    assert_eq!(w.count("order revert"), 0, "the staged release stays");
    assert_eq!(
        w.count("order confirm"),
        1,
        "the confirm is not sent a second time"
    );
    assert_eq!(w.holders(), vec![Slot::B]);
    assert_eq!(w.count("kill"), 0);
    assert_eq!(w.count("terminate pid 100"), 1, "the old child retires");
}

#[test]
fn at_phase_nodes_the_confirm_has_landed_and_a_dead_new_child_is_started_again() {
    // Defect: the boundary moved off `nodes` (a `confirmed` that needs `draining`) and a
    // crash in `nodes` rolled back although the root's `current` names the new release.
    let w = crashed_at(Phase::Nodes, true);
    assert_eq!(w.st.borrow().installed, Some(c(B)), "the root confirmed");
    w.kill_pid(101);
    w.busy(Slot::A, 1, Some(secs(900)));
    let (end, _) = restart(&w);
    assert_post_confirm_restart(&w, end);
    assert_eq!(since_restart(&w, "admin:stand_down A"), 1);
}

#[test]
fn at_phase_baking_the_confirm_has_not_landed_and_a_dead_new_child_rolls_back() {
    // Defect: a bake treated as confirmed (the boundary moved to `baking`): the restart
    // started the new release again although the root's `current` still names the old one.
    let w = crashed_at(Phase::Baking, true);
    assert_eq!(w.st.borrow().installed, Some(c(A)), "not confirmed yet");
    w.kill_pid(101);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.count("admin:resume A"), 1);
    assert_eq!(
        w.count("spawn B"),
        1,
        "the new release is not started again"
    );
    assert_eq!(w.count("order confirm"), 0);
    assert_eq!(w.holders(), vec![Slot::A]);
}

#[test]
fn a_confirm_that_landed_before_update_json_recorded_it_is_not_rolled_back_with_nodes() {
    // Defect: phase still `baking` (the `nodes` record comes after the confirm) was read
    // as before the confirm, so the restart rolled back while `current` names the new one.
    let w = crashed_after_confirm(true);
    assert_eq!(w.last_intent().phase, Phase::Baking);
    assert_eq!(w.st.borrow().installed, Some(c(B)));
    w.kill_pid(101);
    w.busy(Slot::A, 1, Some(secs(900)));
    let (end, notes) = restart(&w);
    assert_post_confirm_restart(&w, end);
    assert_eq!(
        w.count("admin:desired"),
        1,
        "the nodes are sent after a confirm that update.json missed"
    );
    assert!(
        notes.iter().any(|n| n.contains("the root wins")),
        "the disagreement is recorded: {notes:?}"
    );
}

#[test]
fn a_confirm_that_landed_before_the_drain_was_recorded_is_not_rolled_back_without_nodes() {
    // Defect: with `nodes` false the phase goes `baking` -> `draining`, and a crash in
    // between was treated as before the confirm.
    let w = crashed_after_confirm(false);
    assert_eq!(w.last_intent().phase, Phase::Baking);
    w.kill_pid(101);
    w.busy(Slot::A, 1, Some(secs(900)));
    let (end, notes) = restart(&w);
    assert_post_confirm_restart(&w, end);
    assert_eq!(w.count("admin:desired"), 0, "this update sends no nodes");
    assert!(notes.iter().any(|n| n.contains("the root wins")));
}

#[test]
fn a_live_new_child_after_an_unrecorded_confirm_is_not_baked_or_confirmed_again() {
    // Defect: the bake and the confirm ran again: the root, which dropped its staged
    // release, would refuse a second confirm and the update would end `failed`.
    let w = crashed_after_confirm(true);
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.count("order confirm"), 1);
    assert_eq!(w.last_intent().failure, None);
    assert_eq!(w.count("terminate pid 101"), 0);
    assert_eq!(w.count("terminate pid 100"), 1);
}

#[test]
fn a_failed_confirm_try_followed_by_one_that_landed_is_cleared_by_the_root() {
    // Defect: `failure{confirm}` from the first try stayed, so the update ended `failed`
    // and a dead new child rolled back although the second try had landed.
    let (w, old) = World::with_old();
    w.st.borrow_mut().confirm_fails = 1;
    w.crash_at.set(Some(CrashPoint::AfterConfirmLanded));
    run_to_crash(&w, old, true);
    assert_eq!(failure_step(&w), Some(Step::Confirm));
    w.kill_pid(101);
    w.busy(Slot::A, 1, Some(secs(900)));
    let (end, _) = restart(&w);
    assert_eq!(end, Ok(Phase::Done), "{:#?}", w.log());
    assert_eq!(w.last_intent().failure, None);
    assert_eq!(w.count("admin:resume"), 0);
    assert_eq!(w.count("spawn B"), 2);
}

#[test]
fn update_json_saying_confirmed_while_the_root_says_not_rolls_back() {
    // Defect: the recorded phase outvoted the store. The root's `current` is the old
    // release (the order was lost), so the switch is rolled back as before the confirm.
    let w = crashed_at(Phase::Draining, true);
    w.st.borrow_mut().installed = Some(c(A));
    w.kill_pid(101);
    let (end, notes) = restart(&w);
    assert_eq!(end, Ok(Phase::Failed), "{:#?}", w.log());
    assert_eq!(failure_step(&w), Some(Step::Switch));
    assert_eq!(w.count("admin:resume A"), 1);
    assert_eq!(w.count("spawn B"), 1);
    assert!(
        notes.iter().any(|n| n.contains("the root wins")),
        "{notes:?}"
    );
}

#[test]
fn an_unreadable_root_ends_failed_at_restart_and_touches_no_child() {
    // Defect: a guess. Whichever way the restart guessed, a wrong one resumes the old
    // release over a confirmed one or starts the new one over an unconfirmed one.
    for phase in [Phase::Baking, Phase::Nodes, Phase::Draining] {
        let w = crashed_at(phase, true);
        w.st.borrow_mut().installed_err = true;
        w.kill_pid(101);
        let (end, _) = restart(&w);
        assert_eq!(end, Ok(Phase::Failed), "{phase:?}: {:#?}", w.log());
        assert_eq!(failure_step(&w), Some(Step::Restart), "{phase:?}");
        assert!(!w.last_intent().rolling_back, "{phase:?}");
        for call in [
            "spawn",
            "terminate",
            "kill",
            "admin:resume",
            "admin:stand_down",
            "admin:takeover",
            "admin:quiesce",
            "order revert",
            "order confirm",
        ] {
            let n = since_restart(&w, call);
            assert_eq!(n, 0, "{phase:?}: {call} after the restart");
        }
        assert!(w.alive(100), "{phase:?}: the old child is left alone");
    }
}
