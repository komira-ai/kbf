//! The all-in-one swap: hold, quiesce, stop, migrate, start, release.

use super::fakes::*;
use super::model::*;
use super::swap::Swap;
use crate::testkit::{A, c};

fn go(w: &World, old: ProcIdent) -> (Result<Phase, Halt>, Vec<String>) {
    let mut s = Swap::begin(w.ports(), plan(Mode::AllInOne), Some(old)).expect("begin");
    let end = s.run();
    (end, s.notes().to_vec())
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

#[test]
fn the_old_process_stops_only_after_it_is_idle_and_the_held_streams_feed_the_new_one() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(40);
    w.hold_streams(200);
    assert_eq!(go(&w, old).0, Ok(Phase::Done));
    seq(
        &w,
        &[
            "hold",
            "admin:quiesce A",
            "terminate pid 100",
            "migrate",
            "spawn B",
            "release_into B 200",
        ],
    );
    let stopped = w.when("terminate pid 100").unwrap();
    assert!(
        stopped >= secs(40),
        "stopped at {stopped:?} before it was quiesced"
    );
    assert_eq!(w.st.borrow().expired, 0);
    assert_eq!(w.st.borrow().fed, vec![(Slot::B, 200)]);
    assert!(w.when("order confirm").is_some() && w.when("admin:desired B").is_some());
}

#[test]
fn a_hold_that_outlasts_300_seconds_fails_200_streams_and_the_update_continues() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(400);
    w.hold_streams(200);
    assert_eq!(
        go(&w, old).0,
        Ok(Phase::Done),
        "expiry does not abort the update"
    );
    assert_eq!(w.st.borrow().expired, 200);
    assert_eq!(
        w.st.borrow().fed,
        vec![(Slot::B, 0)],
        "no stale stream is fed to the new process"
    );
}

#[test]
fn an_abort_before_the_old_process_exits_resumes_it_and_feeds_the_held_streams_back() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(10_000);
    w.hold_streams(30);
    w.request_at(secs(60), Request::Abort);
    let (end, _) = go(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Abort));
    seq(
        &w,
        &["admin:quiesce A", "admin:resume A", "release_into A 30"],
    );
    assert_eq!(w.count("terminate"), 0);
    assert_eq!(w.count("spawn") + w.count("migrate"), 0);
    assert_eq!(w.state_of(100), RoleState::Active);
}

#[test]
fn an_abort_after_the_old_process_was_told_to_stop_is_refused() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(1);
    w.st.borrow_mut().term_takes = secs(20);
    w.request_at(secs(5), Request::Abort);
    let (end, notes) = go(&w, old);
    assert_eq!(end, Ok(Phase::Done), "the old process was already stopping");
    assert!(
        notes.iter().any(|n| n.contains("abort refused")),
        "{notes:?}"
    );
    assert_eq!(w.count("admin:resume"), 0);
}

#[test]
fn a_new_process_that_never_comes_up_is_replaced_by_the_old_release() {
    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: None,
    });
    w.hold_streams(7);
    let (end, _) = go(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Ready));
    seq(
        &w,
        &[
            "terminate pid 100",
            "spawn B",
            "kill pid 101",
            "spawn A",
            "release_into A 7",
        ],
    );
    assert_eq!(w.st.borrow().active, Slot::A);
    assert_eq!(w.count("order revert"), 1);
}

#[test]
fn a_failed_migration_after_the_old_process_exited_restarts_the_old_release() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().migrate_err = Some("lock timeout".into());
    let (end, _) = go(&w, old);
    assert_eq!(end, Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Migrate));
    assert_eq!(w.count("spawn B"), 0, "the new release never started");
    seq(&w, &["migrate", "spawn A", "release_into A"]);
    assert_eq!(
        w.st.borrow().procs.last().map(|p| p.commit.clone()),
        Some(c(A))
    );
}

#[test]
fn a_restart_while_the_old_process_quiesces_resumes_it() {
    let (w, old) = World::with_old();
    w.st.borrow_mut().quiesce_takes = secs(10_000);
    *w.crash_on_record.borrow_mut() = Some(Box::new(|i| i.phase == Phase::Quiescing));
    let r = Swap::begin(w.ports(), plan(Mode::AllInOne), Some(old)).and_then(|mut s| s.run());
    assert!(matches!(r, Err(Halt::Crashed(_))));
    w.proxy_restart();
    let intent = w.last_intent();
    let mut s = Swap::adopt(
        w.ports(),
        Plan::resumed(&intent, Timing::all_in_one()),
        intent,
    );
    assert_eq!(s.run(), Ok(Phase::Failed));
    assert_eq!(w.last_intent().failure.map(|f| f.step), Some(Step::Restart));
    assert_eq!(w.state_of(100), RoleState::Active);
    assert_eq!(w.count("terminate"), 0);
}

#[test]
fn a_restart_after_the_old_process_was_told_to_stop_goes_forward() {
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
    assert_eq!(w.count("admin:resume"), 0);
    seq(
        &w,
        &["terminate pid 100", "migrate", "spawn B", "release_into B"],
    );
}

/// Catches an all-in-one swap that starts the new process waiting for a takeover: nothing
/// calls `takeover` on this host (the old process is gone), so a process started with
/// `--await-takeover` would serve nothing for ever. Both the new release and the old one put
/// back after a failed start are started to serve.
#[test]
fn the_all_in_one_processes_are_started_to_serve_not_to_wait() {
    let (w, old) = World::with_old();
    assert_eq!(go(&w, old).0, Ok(Phase::Done));
    assert_eq!(w.st.borrow().starts, vec![(Slot::B, Start::Active)]);
    assert_eq!(w.count("admin:takeover"), 0);

    let (w, old) = World::with_old();
    w.script(Script {
        ready_after: None,
        exit_after: None,
    });
    assert_eq!(go(&w, old).0, Ok(Phase::Failed));
    assert_eq!(
        w.st.borrow().starts,
        vec![(Slot::B, Start::Active), (Slot::A, Start::Active)],
        "the new release, then the old one again"
    );
}
