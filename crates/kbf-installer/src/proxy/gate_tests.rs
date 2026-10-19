//! The two calls the real gate added to U7a's `Gate`: `clear_active` and `retire`. A child
//! that served is signalled only after the gate closed its slot, and an all-in-one host
//! starts nothing until the stopped process's slot is closed. Each test names the defect it
//! catches; the mutants planted to see it red are listed in the PR.

use super::fakes::*;
use super::model::*;
use super::swap::Swap;

fn frontend(w: &World, old: ProcIdent) -> Result<Phase, Halt> {
    Swap::begin(w.ports(), plan(Mode::Frontend), Some(old))?.run()
}

fn aio(w: &World, old: ProcIdent) -> (Result<Phase, Halt>, Vec<String>) {
    let mut s = Swap::begin(w.ports(), plan(Mode::AllInOne), Some(old)).expect("begin");
    let end = s.run();
    (end, s.notes().to_vec())
}

#[test]
fn a_child_whose_own_counts_are_zero_is_not_signalled_while_the_gate_says_busy() {
    // The child is idle at once; the proxy still has streams bound to its slot until 700 s.
    // The permits clear last, so only the gate's answer can hold the SIGTERM back.
    let (w, old) = World::with_old();
    w.permits(Slot::A, 3, Some(secs(700)));
    assert_eq!(frontend(&w, old), Ok(Phase::Done));
    let signalled = w.when("terminate pid 100").expect("the old child is retired");
    assert!(
        signalled >= secs(700),
        "SIGTERM at {signalled:?} with streams still bound"
    );
    assert!(w.count("gate:retire A Busy(3)") > 0, "{:#?}", w.log());
}

#[test]
fn the_sigterm_follows_the_gates_close_and_is_sent_once() {
    let (w, old) = World::with_old();
    assert_eq!(frontend(&w, old), Ok(Phase::Done));
    let log = w.log();
    let closed = log.iter().position(|l| l == "gate:retire A Closed").expect("closed");
    let term = log.iter().position(|l| l == "terminate pid 100").expect("signalled");
    assert!(closed < term, "{log:#?}");
    assert_eq!(w.count("terminate pid 100"), 1);
}

#[test]
fn an_active_slot_is_never_retired() {
    // The fake gate, like the real one, answers `Current` for the slot that serves; the
    // swap must not read that as closed. A child on the active slot is not signalled.
    let (w, old) = World::with_old();
    w.st.borrow_mut().active = Slot::A;
    assert_eq!(w.ports().gate.retire(Slot::A), Retired::Current);
    assert_eq!(frontend(&w, old), Ok(Phase::Done));
    assert_eq!(w.st.borrow().active, Slot::B);
}

#[test]
fn an_all_in_one_host_clears_the_active_slot_and_waits_for_the_close_before_it_starts() {
    // The stopped process's streams drain 25 s after the stop. The new process is not
    // started before the slot is closed; until then no slot is active.
    let (w, old) = World::with_old();
    w.permits(Slot::A, 4, Some(secs(25)));
    let (end, _) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    let log = w.log();
    let at = |what: &str| {
        log.iter()
            .position(|l| l.starts_with(what))
            .unwrap_or_else(|| panic!("`{what}` is not in {log:#?}"))
    };
    assert!(at("terminate pid 100") < at("clear_active"));
    assert!(at("clear_active") < at("gate:retire A Busy"));
    assert!(at("gate:retire A Closed") < at("migrate"));
    assert!(at("migrate") < at("spawn B"));
    let spawned = w.when("spawn B").expect("started");
    assert!(spawned >= secs(25), "started at {spawned:?}, streams still bound");
}

#[test]
fn a_stopped_slot_that_stays_busy_past_the_hold_bound_is_reported_and_still_waited_for() {
    let (w, old) = World::with_old();
    w.permits(Slot::A, 2, Some(secs(500)));
    let (end, notes) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Done));
    assert!(
        notes.iter().any(|n| n.contains("still has streams bound")),
        "{notes:?}"
    );
    assert_eq!(
        notes.iter().filter(|n| n.contains("still has streams bound")).count(),
        1,
        "reported once, not on every poll"
    );
    assert!(w.when("spawn B").is_some_and(|t| t >= secs(500)));
}

#[test]
fn a_new_process_that_fails_leaves_the_old_slot_closed_until_the_old_release_is_released_into() {
    // The slot was retired after the stop; the old release comes back into it, and the
    // gate opens it again (a new generation) only at the release. The fake gate forgets
    // the retire at `release_into`, as the real one opens the slot there.
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: None,
    });
    let (end, _) = aio(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    let log = w.log();
    let closed = log.iter().position(|l| l == "gate:retire A Closed").expect("closed");
    let back = log.iter().rposition(|l| l.starts_with("release_into A")).expect("released");
    assert!(closed < back, "{log:#?}");
    assert!(w.st.borrow().retired.is_empty(), "A is open again");
    assert_eq!(w.st.borrow().active, Slot::A);
}

#[test]
fn a_restart_after_the_stop_clears_the_active_slot_and_waits_for_the_close_again() {
    let (w, old) = World::with_old();
    *w.crash_on_record.borrow_mut() = Some(Box::new(|i| i.phase == Phase::Switching));
    let r = Swap::begin(w.ports(), plan(Mode::AllInOne), Some(old)).and_then(|mut s| s.run());
    assert!(matches!(r, Err(Halt::Crashed(_))));
    w.proxy_restart();
    let intent = w.last_intent();
    let mut s = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::all_in_one()),
        intent,
    );
    assert_eq!(s.run(), Ok(Phase::Done));
    assert!(w.count("clear_active") >= 1);
    assert!(w.pos("gate:retire A Closed").unwrap() < w.pos("spawn B").unwrap());
}
