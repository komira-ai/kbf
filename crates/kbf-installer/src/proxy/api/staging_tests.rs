//! The staging deadline and the restart that keeps it: an accepted update the installer
//! never stages ends `failed` with a `revert` behind it, from the instant its order was
//! written, however many times the process restarts in between.
//!
//! The clock is the rig's fake one: `at(&rig, secs)` puts it `secs` seconds after the
//! start, and an update posted at the start has a deadline at 900 s.

use std::sync::atomic::Ordering;

use hyper::StatusCode;

use super::core::{ApiError, Config, Core, DeadlineRange, Fault, Reply, StagingDeadline};
use super::fakes::{COMMIT_C, Rig, idle, req, staged};
use super::idempotency::Idempotency;
use super::model::{StatusBody, UpdateId};
use super::traits::{Compat, InstallerState, InstallerStatus};
use crate::proxy::model::{Failure, Phase, Step, Verb};
use crate::testkit::{A, B, c};

/// The fake clock's Unix second at `at(&rig, 0)`.
const START: u64 = 1_800_000_000;

fn accepted(r: Result<Reply, Fault>) -> (UpdateId, Phase) {
    match r {
        Ok(Reply::Update { update_id, phase }) => (update_id, phase),
        other => panic!("expected an update reply, got {other:?}"),
    }
}

fn refused(r: Result<Reply, Fault>) -> ApiError {
    match r {
        Err(Fault::Api(e)) => e,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn body(core: &mut Core) -> StatusBody {
    match core.status() {
        Reply::Status(b) => *b,
        other => panic!("expected a status, got {other:?}"),
    }
}

fn written(rig: &Rig) -> Vec<(Verb, String)> {
    rig.orders
        .written
        .lock()
        .unwrap()
        .iter()
        .map(|l| (l.verb, l.commit.to_string()))
        .collect()
}

fn at(rig: &Rig, secs: u64) {
    rig.clock.0.store(secs * 1000, Ordering::SeqCst);
}

fn fetching() -> InstallerStatus {
    InstallerStatus {
        state: InstallerState::Fetching,
        ..idle()
    }
}

fn timeout() -> Failure {
    Failure {
        step: Step::Stage,
        reason: "timeout".to_owned(),
    }
}

/// An update for commit B accepted at the start, order 101.
fn started(rig: &Rig) -> (Core, UpdateId) {
    let mut core = rig.core();
    let (id, _) = accepted(core.post(req(B, "r1")));
    (core, id)
}

fn order_then_revert() -> Vec<(Verb, String)> {
    vec![
        (Verb::FrontendUpdate, B.to_owned()),
        (Verb::Revert, B.to_owned()),
    ]
}

#[test]
fn an_update_the_installer_never_stages_fails_at_the_deadline_and_a_revert_follows() {
    let rig = Rig::new("u8b8-deadline");
    let (mut core, id) = started(&rig);
    at(&rig, 899);
    let s = body(&mut core);
    assert_eq!(s.phase, Some(Phase::Accepted), "one second short");
    assert_eq!(written(&rig).len(), 1);
    at(&rig, 900);
    let s = body(&mut core);
    assert_eq!(
        (s.update_id, s.phase, s.failure),
        (Some(id.clone()), Some(Phase::Failed), Some(timeout()))
    );
    assert_eq!(s.since, Some(START + 900), "it ended at the deadline");
    assert_eq!(written(&rig), order_then_revert());
    assert_eq!(rig.sup.starts(), 0, "no child");
    // Looking at the ended update again writes no second revert.
    at(&rig, 1_500);
    body(&mut core);
    assert_eq!(written(&rig).len(), 2);
    // The retry of its request finds how it ended.
    assert_eq!(accepted(core.post(req(B, "r1"))), (id, Phase::Failed));
    let revert = rig.orders.written.lock().unwrap()[1].clone();
    assert_eq!((revert.commit, revert.window_s), (c(B), 900));
}

#[test]
fn after_a_timeout_the_next_update_waits_for_the_installer_to_let_go() {
    let rig = Rig::new("u8b8-next");
    let (mut core, _) = started(&rig);
    rig.set_status(fetching());
    at(&rig, 900);
    assert_eq!(body(&mut core).phase, Some(Phase::Failed));
    // The installer has not yet acted on the revert: the new update is refused and writes
    // nothing, as for any busy installer.
    let e = refused(core.post(req(COMMIT_C, "r2")));
    assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "installer_busy"));
    assert_eq!(written(&rig).len(), 2);
    // It has: the next update is accepted.
    rig.set_status(idle());
    let (id, phase) = accepted(core.post(req(COMMIT_C, "r2")));
    assert_eq!((id.0.as_str(), phase), ("upd-2", Phase::Accepted));
    assert_eq!(
        written(&rig)[2],
        (Verb::FrontendUpdate, COMMIT_C.to_owned())
    );
}

#[test]
fn a_fetch_under_way_does_not_move_the_deadline() {
    let rig = Rig::new("u8b8-fetching");
    let (mut core, _) = started(&rig);
    rig.set_status(fetching());
    at(&rig, 600);
    let s = body(&mut core);
    assert_eq!(
        (s.phase, s.since),
        (Some(Phase::Fetching), Some(START + 600))
    );
    at(&rig, 899);
    assert_eq!(body(&mut core).phase, Some(Phase::Fetching));
    at(&rig, 900);
    let s = body(&mut core);
    assert_eq!((s.phase, s.failure), (Some(Phase::Failed), Some(timeout())));
}

#[test]
fn a_release_staged_just_before_the_deadline_goes_on() {
    let rig = Rig::new("u8b8-just-in-time");
    let (mut core, id) = started(&rig);
    at(&rig, 899);
    rig.set_status(staged(B));
    let s = body(&mut core);
    assert_eq!((s.update_id, s.phase), (Some(id), Some(Phase::Preflight)));
    assert_eq!(rig.sup.starts(), 1);
    // The deadline is not the supervisor's: long after it, the API writes no revert.
    at(&rig, 5_000);
    assert_eq!(body(&mut core).phase, Some(Phase::Preflight));
    assert_eq!(written(&rig).len(), 1);
    assert_eq!(rig.sup.starts(), 1);
}

#[test]
fn a_release_found_staged_after_the_deadline_is_used_not_reverted() {
    let rig = Rig::new("u8b8-late-stage");
    let (mut core, _) = started(&rig);
    at(&rig, 1_000);
    rig.set_status(staged(B));
    assert_eq!(body(&mut core).phase, Some(Phase::Preflight));
    assert_eq!(rig.sup.starts(), 1);
    assert_eq!(written(&rig).len(), 1, "the work was done: no revert");
}

#[test]
fn a_restarted_process_goes_on_waiting_with_the_deadline_of_the_first() {
    let rig = Rig::new("u8b8-restart");
    let (core, id) = started(&rig);
    drop(core);
    at(&rig, 600);
    let mut restarted = rig.core();
    let s = body(&mut restarted);
    assert_eq!(
        (s.update_id, s.phase),
        (Some(id.clone()), Some(Phase::Accepted))
    );
    assert_eq!(s.from, Some(c(A)));
    assert_eq!(s.since, Some(START), "it began when the order was written");
    // Its request finds it; a second update is refused; no order is written again.
    assert_eq!(
        accepted(restarted.post(req(B, "r1"))),
        (id, Phase::Accepted)
    );
    assert_eq!(
        refused(restarted.post(req(COMMIT_C, "r2"))).code,
        "update_open"
    );
    assert_eq!(written(&rig).len(), 1);
    at(&rig, 899);
    assert_eq!(body(&mut restarted).phase, Some(Phase::Accepted));
    at(&rig, 900);
    let s = body(&mut restarted);
    assert_eq!((s.phase, s.failure), (Some(Phase::Failed), Some(timeout())));
    assert_eq!(written(&rig), order_then_revert());
}

#[test]
fn a_restarted_process_hands_over_what_the_first_one_waited_for() {
    let rig = Rig::new("u8b8-restart-handover");
    let (core, id) = started(&rig);
    drop(core);
    let mut restarted = rig.core();
    rig.set_status(staged(B));
    assert_eq!(body(&mut restarted).phase, Some(Phase::Preflight));
    let spec = rig.sup.0.lock().unwrap().starts[0].clone();
    assert_eq!(
        (spec.update_id, spec.request_id.as_str(), spec.from, spec.to),
        (id, "r1", Some(c(A)), c(B))
    );
    assert_eq!(
        (spec.nodes, spec.rollback, spec.order.order_id),
        (true, false, 101)
    );
    assert_eq!(written(&rig).len(), 1);
}

#[test]
fn a_process_restarted_after_the_deadline_ends_the_update_on_its_first_look() {
    let rig = Rig::new("u8b8-restart-late");
    let (core, _) = started(&rig);
    drop(core);
    at(&rig, 1_200);
    let mut restarted = rig.core();
    let s = body(&mut restarted);
    assert_eq!((s.phase, s.failure), (Some(Phase::Failed), Some(timeout())));
    assert_eq!(written(&rig), order_then_revert());
}

#[test]
fn an_update_taken_up_after_a_restart_can_be_aborted() {
    let rig = Rig::new("u8b8-restart-abort");
    let (core, id) = started(&rig);
    drop(core);
    let mut restarted = rig.core();
    assert_eq!(accepted(restarted.abort()), (id, Phase::RolledBack));
    assert_eq!(written(&rig), order_then_revert());
    assert!(Idempotency::open(&rig.dir).waiting().is_none());
}

#[test]
fn a_restarted_process_does_not_take_up_an_update_the_supervisor_has() {
    let rig = Rig::new("u8b8-restart-owned");
    let (core, id) = started(&rig);
    drop(core);
    // The first process handed the update over and died before it recorded that: the
    // map still says the update waits, and the supervisor has it.
    rig.sup.set(Phase::Preflight, None, 0);
    rig.sup
        .0
        .lock()
        .unwrap()
        .status
        .update
        .as_mut()
        .unwrap()
        .update_id = id;
    let mut restarted = rig.core();
    assert!(Idempotency::open(&rig.dir).waiting().is_none());
    at(&rig, 5_000);
    assert_eq!(body(&mut restarted).phase, Some(Phase::Preflight));
    assert_eq!(
        written(&rig).len(),
        1,
        "no revert for the supervisor's update"
    );
    assert_eq!(rig.sup.starts(), 0, "and no second start");
}

#[test]
fn every_way_an_update_leaves_the_api_ends_its_wait_on_disk() {
    type Leave = fn(&Rig, &mut Core);
    let cases: [(&str, Leave); 6] = [
        ("handed over", |rig, core| {
            rig.set_status(staged(B));
            body(core);
        }),
        ("refused", |rig, core| {
            rig.set_status(InstallerStatus {
                state: InstallerState::Refused,
                refusal: "hash: the digest differs".to_owned(),
                refused_order_id: Some(101),
                ..idle()
            });
            body(core);
        }),
        ("aborted", |_, core| {
            accepted(core.abort());
        }),
        ("incompatible", |rig, core| {
            *rig.gate.staged.lock().unwrap() = Compat::Incompatible {
                rule: "R1".to_owned(),
            };
            rig.set_status(staged(B));
            body(core);
        }),
        ("supervisor cannot start", |rig, core| {
            rig.sup.0.lock().unwrap().start_fails = true;
            rig.set_status(staged(B));
            body(core);
        }),
        ("timed out", |rig, core| {
            at(rig, 900);
            body(core);
        }),
    ];
    let mut wrong = Vec::new();
    for (i, (name, leave)) in cases.into_iter().enumerate() {
        let rig = Rig::new(&format!("u8b8-leave-{i}"));
        let (mut core, _) = started(&rig);
        if Idempotency::open(&rig.dir).waiting().is_none() {
            wrong.push(format!("{name}: not waiting while staging"));
        }
        leave(&rig, &mut core);
        if Idempotency::open(&rig.dir).waiting().is_some() {
            wrong.push(format!("{name}: still waiting on disk"));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

#[test]
fn an_end_that_cannot_be_saved_still_ends_the_update() {
    let mut rig = Rig::new("u8b8-unsaved-end");
    let outer = rig.dir.clone();
    rig.dir = outer.join("state");
    std::fs::create_dir(&rig.dir).unwrap();
    let (mut core, id) = started(&rig);
    std::fs::rename(&rig.dir, outer.join("moved")).unwrap();
    assert_eq!(accepted(core.abort()), (id, Phase::RolledBack));
    assert_eq!(body(&mut core).phase, Some(Phase::RolledBack));
}

#[test]
fn a_deadline_further_off_than_a_deadline_is_long_counts_from_the_restart() {
    let rig = Rig::new("u8b8-clock-back");
    at(&rig, 3_600);
    let (core, _) = started(&rig);
    drop(core);
    // The clock is an hour behind where the first process left it.
    at(&rig, 0);
    let mut restarted = rig.core();
    at(&rig, 899);
    assert_eq!(body(&mut restarted).phase, Some(Phase::Accepted));
    at(&rig, 900);
    assert_eq!(body(&mut restarted).phase, Some(Phase::Failed));
}

#[test]
fn a_timeout_whose_revert_cannot_be_written_keeps_the_update_open_until_it_can() {
    let rig = Rig::new("u8b8-revert-fails");
    let (mut core, id) = started(&rig);
    *rig.orders.fail.lock().unwrap() = true;
    for secs in [900, 950] {
        at(&rig, secs);
        let s = body(&mut core);
        assert_eq!(
            (s.update_id, s.phase, s.failure),
            (Some(id.clone()), Some(Phase::Accepted), None)
        );
    }
    assert_eq!(written(&rig).len(), 1);
    // It stays the open update, so nothing else starts behind a staged release nobody
    // tracks.
    assert_eq!(refused(core.post(req(COMMIT_C, "r2"))).code, "update_open");
    *rig.orders.fail.lock().unwrap() = false;
    let s = body(&mut core);
    assert_eq!((s.phase, s.failure), (Some(Phase::Failed), Some(timeout())));
    assert_eq!(written(&rig), order_then_revert());
}

#[test]
fn an_unreadable_status_file_does_not_hold_an_update_past_its_deadline() {
    let rig = Rig::new("u8b8-unreadable-deadline");
    let (mut core, id) = started(&rig);
    *rig.status.0.lock().unwrap() = Err("torn".to_owned());
    at(&rig, 899);
    assert_eq!(
        body(&mut core).phase,
        Some(Phase::Accepted),
        "one second short"
    );
    assert_eq!(written(&rig).len(), 1);
    at(&rig, 900);
    let s = body(&mut core);
    assert_eq!(
        (s.update_id, s.phase, s.failure),
        (Some(id), Some(Phase::Failed), Some(timeout()))
    );
    assert_eq!(written(&rig), order_then_revert());
    // The file comes back: the ended update stays ended and nothing more is written.
    rig.set_status(idle());
    at(&rig, 1_000);
    assert_eq!(body(&mut core).phase, Some(Phase::Failed));
    assert_eq!(written(&rig).len(), 2);
}

#[test]
fn the_deadline_is_bounded_below_the_age_an_order_lives_in_the_spool() {
    let max = kbf_update::MAX_AGE_SECS;
    assert_eq!(StagingDeadline::MAX_S, max - 1);
    assert_eq!(StagingDeadline::from_secs(0), Err(DeadlineRange(0)));
    assert_eq!(
        StagingDeadline::from_secs(1).map(StagingDeadline::secs),
        Ok(1)
    );
    assert_eq!(
        StagingDeadline::from_secs(max - 1).map(StagingDeadline::secs),
        Ok(max - 1)
    );
    assert_eq!(StagingDeadline::from_secs(max), Err(DeadlineRange(max)));
    assert_eq!(StagingDeadline::DEFAULT.secs(), 900, "fifteen minutes");
    assert_eq!(Config::default().staging_deadline, StagingDeadline::DEFAULT);
    assert_eq!(
        DeadlineRange(2000).to_string(),
        "a staging deadline is 1 to 1799 seconds, below the 1800 seconds an order lives in the \
         spool; got 2000"
    );
}

#[test]
fn a_configured_deadline_is_the_one_that_counts() {
    let rig = Rig::new("u8b8-configured");
    let mut core = rig.core();
    core.set_config(Config {
        staging_deadline: StagingDeadline::from_secs(60).unwrap(),
        ..Config::default()
    });
    accepted(core.post(req(B, "r1")));
    let waiting = Idempotency::open(&rig.dir).waiting().unwrap().1.clone();
    assert_eq!((waiting.since_s, waiting.deadline_s), (START, START + 60));
    at(&rig, 59);
    assert_eq!(body(&mut core).phase, Some(Phase::Accepted));
    at(&rig, 60);
    assert_eq!(body(&mut core).phase, Some(Phase::Failed));
}

#[test]
fn only_a_refusal_that_names_our_order_ends_the_update_with_its_word() {
    // Whatever the floor says: 999 is above our order, 101.
    for (name, named) in [("older", Some(7)), ("newer", Some(102)), ("none", None)] {
        let rig = Rig::new(&format!("u8b8-refusal-{name}"));
        let (mut core, _) = started(&rig);
        rig.set_status(InstallerStatus {
            state: InstallerState::Refused,
            refusal: "hash: someone else's".to_owned(),
            acked_order_id: 999,
            refused_order_id: named,
            ..idle()
        });
        assert_eq!(body(&mut core).phase, Some(Phase::Accepted), "{name}");
        assert_eq!(rig.sup.starts(), 0, "{name}");
        // Not ours, so it does not end the update: the deadline does.
        at(&rig, 900);
        let s = body(&mut core);
        assert_eq!(
            (s.phase, s.failure),
            (Some(Phase::Failed), Some(timeout())),
            "{name}"
        );
    }
    let rig = Rig::new("u8b8-refusal-ours");
    let (mut core, _) = started(&rig);
    rig.set_status(InstallerStatus {
        state: InstallerState::Refused,
        refusal: "hash: the digest differs".to_owned(),
        refused_order_id: Some(101),
        ..idle()
    });
    let f = body(&mut core).failure.expect("failure");
    assert_eq!((f.step, f.reason.as_str()), (Step::Verify, "hash"));
}
