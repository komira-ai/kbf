//! The state machine behind the routes, against fakes. Router-level behavior (status
//! codes, bodies, auth, forwarding, the crash retry through the actor) is in
//! `router_tests.rs`.

use std::sync::atomic::Ordering;

use hyper::StatusCode;

use super::core::{ApiError, Core, Crash, CrashHook, CrashPoint, Fault, NoCrash, Reply};
use super::fakes::{COMMIT_C, Rig, facts, idle, req, staged};
use super::model::{OldChild, RollbackBlocked, StatusBody, UpdateId, abortable};
use super::refusal;
use super::traits::{
    Compat, CompatGate, InstallerState, InstallerStatus, JsonFileStatus, StatusReader, StubGate,
    SupervisorError,
};
use crate::proxy::model::{Phase, Step, Verb};
use crate::testkit::{A, B, c};

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

fn old() -> OldChild {
    OldChild {
        open_streams: 4,
        oldest_stream_age_s: 90,
        open_executions: 2,
        queued_operations: 1,
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

/// An update accepted for commit B, order 101.
fn started(rig: &Rig) -> (Core, UpdateId) {
    let mut core = rig.core();
    let (id, _) = accepted(core.post(req(B, "r1")));
    (core, id)
}

#[test]
fn post_accepts_and_writes_one_locator_order_and_starts_nothing() {
    let rig = Rig::new("u8a-post");
    let mut core = rig.core();
    let (id, phase) = accepted(core.post(req(B, "r1")));
    assert_eq!(phase, Phase::Accepted);
    assert_eq!(written(&rig), vec![(Verb::FrontendUpdate, B.to_owned())]);
    assert_eq!(rig.orders.written.lock().unwrap()[0].window_s, 900);
    assert_eq!(rig.sup.starts(), 0);
    let s = body(&mut core);
    assert_eq!(s.update_id, Some(id));
    assert_eq!(s.from, Some(c(A)));
    assert_eq!(s.to, Some(c(B)));
    assert_eq!(s.phase, Some(Phase::Accepted));
}

#[test]
fn same_request_id_returns_the_same_update_without_a_second_order() {
    let rig = Rig::new("u8a-idem");
    let mut core = rig.core();
    let (first, _) = accepted(core.post(req(B, "r1")));
    let (again, phase) = accepted(core.post(req(B, "r1")));
    assert_eq!(first, again);
    assert_eq!(phase, Phase::Accepted);
    assert_eq!(written(&rig).len(), 1);
}

#[test]
fn a_request_id_reused_for_another_request_is_refused() {
    let rig = Rig::new("u8a-reuse");
    let mut core = rig.core();
    accepted(core.post(req(B, "r1")));
    let e = refused(core.post(req(COMMIT_C, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::CONFLICT, "request_id_reused")
    );
    let mut other = req(B, "r1");
    other.nodes = false;
    assert_eq!(refused(core.post(other)).code, "request_id_reused");
}

#[test]
fn bad_request_ids_are_refused() {
    let rig = Rig::new("u8a-badid");
    let mut core = rig.core();
    for id in ["", "has space", "slash/x", &"x".repeat(129)] {
        let e = refused(core.post(req(B, id)));
        assert_eq!(
            (e.status, e.code),
            (StatusCode::BAD_REQUEST, "bad_request_id")
        );
    }
    assert!(written(&rig).is_empty());
}

#[test]
fn a_second_update_while_one_is_open_is_a_409_and_writes_nothing() {
    let rig = Rig::new("u8a-open");
    let (mut core, _) = started(&rig);
    let e = refused(core.post(req(COMMIT_C, "r2")));
    assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "update_open"));
    assert_eq!(written(&rig).len(), 1);
    // The refused request left no trace: once the first ends, r2 is a fresh request.
    accepted(core.abort());
    let (_, phase) = accepted(core.post(req(COMMIT_C, "r2")));
    assert_eq!(phase, Phase::Accepted);
}

#[test]
fn an_update_open_with_the_supervisor_blocks_a_new_one_too() {
    let rig = Rig::new("u8a-open-sup");
    rig.sup.set(Phase::Baking, Some(old()), 0);
    let e = refused(rig.core().post(req(COMMIT_C, "r1")));
    assert_eq!(e.code, "update_open");
}

#[test]
fn an_old_child_still_draining_is_a_409() {
    let rig = Rig::new("u8a-drain");
    rig.sup.0.lock().unwrap().status.old_child_draining = true;
    let e = refused(rig.core().post(req(B, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::CONFLICT, "old_child_draining")
    );
    assert!(written(&rig).is_empty());
}

#[test]
fn the_running_commit_is_a_409() {
    let rig = Rig::new("u8a-same");
    let e = refused(rig.core().post(req(A, "r1")));
    assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "same_commit"));
    // The supervisor's running commit wins over status.json's installed commit.
    rig.sup.0.lock().unwrap().status.running = Some(c(B));
    assert_eq!(refused(rig.core().post(req(B, "r2"))).code, "same_commit");
    assert!(written(&rig).is_empty());
}

#[test]
fn an_incompatible_release_is_a_409_naming_the_rule() {
    let rig = Rig::new("u8a-incompat");
    *rig.gate.before.lock().unwrap() = Compat::Incompatible {
        rule: "R1".to_owned(),
    };
    let e = refused(rig.core().post(req(B, "r1")));
    assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "incompatible"));
    assert_eq!(e.extra, Some(("rule", "R1".to_owned())));
    assert!(written(&rig).is_empty());
    assert_eq!(rig.sup.starts(), 0);
}

#[test]
fn a_release_incomplete_for_the_platform_is_a_422() {
    let rig = Rig::new("u8a-incomplete");
    *rig.gate.before.lock().unwrap() = Compat::Incomplete {
        reason: "no linux-arm64 tarball".to_owned(),
    };
    let e = refused(rig.core().post(req(B, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::UNPROCESSABLE_ENTITY, "incomplete_release")
    );
    assert!(written(&rig).is_empty());
}

#[test]
fn a_busy_installer_is_a_409_and_an_idle_one_is_not() {
    for (i, state) in [
        InstallerState::Fetching,
        InstallerState::Staged,
        InstallerState::ActivatedUnconfirmed,
        InstallerState::Unknown,
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(&format!("u8a-busy-{i}"));
        rig.set_status(InstallerStatus { state, ..idle() });
        let e = refused(rig.core().post(req(B, "r1")));
        assert_eq!(
            (e.status, e.code),
            (StatusCode::CONFLICT, "installer_busy"),
            "{state:?}"
        );
    }
    for (i, state) in [
        InstallerState::Idle,
        InstallerState::Confirmed,
        InstallerState::Reverted,
        InstallerState::Refused,
    ]
    .into_iter()
    .enumerate()
    {
        let rig = Rig::new(&format!("u8a-quiet-{i}"));
        rig.set_status(InstallerStatus { state, ..idle() });
        accepted(rig.core().post(req(B, "r1")));
    }
}

#[test]
fn an_unreadable_status_file_refuses_a_new_update() {
    let rig = Rig::new("u8a-nostatus");
    *rig.status.0.lock().unwrap() = Err("No such file".to_owned());
    let e = refused(rig.core().post(req(B, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::SERVICE_UNAVAILABLE, "status_unreadable")
    );
}

#[test]
fn a_failed_order_write_is_a_503_and_the_retry_is_the_same_update() {
    let rig = Rig::new("u8a-orderfail");
    let mut core = rig.core();
    *rig.orders.fail.lock().unwrap() = true;
    let e = refused(core.post(req(B, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::SERVICE_UNAVAILABLE, "order_unwritable")
    );
    assert_eq!(body(&mut core).phase, None, "nothing is open");
    *rig.orders.fail.lock().unwrap() = false;
    let (id, phase) = accepted(core.post(req(B, "r1")));
    assert_eq!(phase, Phase::Accepted);
    assert_eq!(id.0, "upd-1");
    assert_eq!(written(&rig).len(), 1);
}

#[test]
fn an_unwritable_request_map_is_a_503_and_no_order_is_written() {
    let rig = Rig::new("u8a-nomap");
    let mut parts_rig = Rig::new("u8a-nomap-parts");
    parts_rig.dir = rig.dir.join("missing");
    let e = refused(parts_rig.core().post(req(B, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::SERVICE_UNAVAILABLE, "state_unwritable")
    );
    assert!(written(&parts_rig).is_empty());
}

#[test]
fn an_unreadable_request_map_refuses_new_updates() {
    let rig = Rig::new("u8a-lost");
    std::fs::write(rig.dir.join("requests.json"), b"{not json").unwrap();
    let e = refused(rig.core().post(req(B, "r1")));
    assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "state_lost"));
    assert!(written(&rig).is_empty());
}

#[test]
fn killed_between_the_save_and_the_answer_the_retry_gets_the_same_update() {
    let rig = Rig::new("u8a-crash");
    *rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
    let mut core = rig.core();
    assert_eq!(core.post(req(B, "r1")), Err(Fault::Crash));
    assert!(
        written(&rig).is_empty(),
        "the process died before the order"
    );
    // A restarted process reads the map from disk.
    let saved = super::idempotency::Idempotency::open(&rig.dir);
    let lost_id = saved
        .get("r1")
        .expect("the map was fsynced before the crash")
        .update_id
        .clone();
    let mut restarted = rig.core();
    let (id, phase) = accepted(restarted.post(req(B, "r1")));
    assert_eq!(id, lost_id);
    assert_eq!(phase, Phase::Accepted);
    assert_eq!(
        written(&rig).len(),
        1,
        "the order is written once, by the retry"
    );
    // And a third try, after yet another restart, still names it.
    let (again, _) = accepted(restarted.post(req(B, "r1")));
    assert_eq!(again, lost_id);
}

#[test]
fn a_request_accepted_before_a_crash_is_not_revived_behind_another_update() {
    let rig = Rig::new("u8a-crash-other");
    *rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
    assert_eq!(rig.core().post(req(B, "r1")), Err(Fault::Crash));
    let mut restarted = rig.core();
    accepted(restarted.post(req(COMMIT_C, "r2")));
    let e = refused(restarted.post(req(B, "r1")));
    assert_eq!(e.code, "request_id_retired");
}

#[test]
fn staging_follows_status_json_to_a_handover() {
    let rig = Rig::new("u8a-handover");
    let (mut core, id) = started(&rig);
    rig.set_status(InstallerStatus {
        state: InstallerState::Fetching,
        ..idle()
    });
    assert_eq!(body(&mut core).phase, Some(Phase::Fetching));
    // Staged, but for another commit: not ours, nothing starts.
    rig.set_status(staged(COMMIT_C));
    assert_eq!(body(&mut core).phase, Some(Phase::Fetching));
    assert_eq!(rig.sup.starts(), 0);
    rig.set_status(staged(B));
    let s = body(&mut core);
    assert_eq!(rig.sup.starts(), 1);
    assert_eq!(
        s.phase,
        Some(Phase::Preflight),
        "the supervisor's phase from here on"
    );
    assert_eq!(s.update_id, Some(id.clone()));
    let spec = rig.sup.0.lock().unwrap().starts[0].clone();
    assert_eq!((spec.update_id, spec.to, spec.from), (id, c(B), Some(c(A))));
    assert_eq!(
        (spec.nodes, spec.rollback, spec.facts),
        (true, false, facts())
    );
    assert_eq!(spec.request_id, "r1", "the swap records the caller's key");
    assert_eq!(spec.order.order_id, 101);
    // Reading again does not hand over twice.
    body(&mut core);
    assert_eq!(rig.sup.starts(), 1);
    assert_eq!(written(&rig).len(), 1, "no revert after a good handover");
}

#[test]
fn an_unreadable_status_file_leaves_a_staging_update_where_it_was() {
    let rig = Rig::new("u8a-stage-unreadable");
    let (mut core, _) = started(&rig);
    *rig.status.0.lock().unwrap() = Err("torn".to_owned());
    assert_eq!(body(&mut core).phase, Some(Phase::Accepted));
}

#[test]
fn every_refusal_word_fails_the_update_and_starts_no_child() {
    for (i, word) in refusal::WORDS.iter().enumerate() {
        let rig = Rig::new(&format!("u8a-refusal-{i}"));
        let (mut core, id) = started(&rig);
        rig.set_status(InstallerStatus {
            state: InstallerState::Refused,
            refusal: format!("{word}: some detail"),
            acked_order_id: 101,
            ..idle()
        });
        let s = body(&mut core);
        assert_eq!(s.update_id, Some(id), "{word}");
        assert_eq!(s.phase, Some(Phase::Failed), "{word}");
        let f = s.failure.expect("a failure");
        assert_eq!((f.step, f.reason.as_str()), (refusal::step(word), *word));
        assert_ne!(f.step, Step::Stage, "{word} has a step of its own");
        assert_eq!(rig.sup.starts(), 0, "{word}: no child");
        assert_eq!(rig.sup.calls(), Vec::<&str>::new(), "{word}");
        assert_eq!(
            written(&rig).len(),
            1,
            "{word}: nothing was staged, nothing to revert"
        );
        // The update is over: a new one is accepted.
        accepted(core.post(req(COMMIT_C, "next")));
    }
}

#[test]
fn each_refusal_word_has_its_step() {
    let table = [
        ("trust_chain", Step::Verify),
        ("hash", Step::Verify),
        ("attestation", Step::Verify),
        ("commit_or_ref", Step::Verify),
        ("facts", Step::Verify),
        ("installer_too_old", Step::Verify),
        ("platform", Step::Verify),
        ("size", Step::Verify),
        ("disk", Step::Fetch),
        ("fetch", Step::Fetch),
        ("replay", Step::Order),
        ("generation", Step::Order),
        ("floor", Step::Order),
        ("denied", Step::Order),
        ("pending", Step::Host),
        ("lease", Step::Host),
        ("envoy_range", Step::Host),
        ("frontend_busy", Step::Host),
    ];
    assert_eq!(table.len(), refusal::WORDS.len());
    for (word, step) in table {
        assert!(refusal::WORDS.contains(&word), "{word}");
        assert_eq!(refusal::step(word), step, "{word}");
    }
}

#[test]
fn an_unknown_refusal_word_fails_closed() {
    let rig = Rig::new("u8a-refusal-unknown");
    let (mut core, _) = started(&rig);
    rig.set_status(InstallerStatus {
        state: InstallerState::Refused,
        refusal: "the restart failed".to_owned(),
        acked_order_id: 101,
        ..idle()
    });
    let f = body(&mut core).failure.expect("failure");
    assert_eq!(
        (f.step, f.reason.as_str()),
        (Step::Stage, "the restart failed")
    );
    assert_eq!(rig.sup.starts(), 0);
}

#[test]
fn a_refusal_of_an_older_order_is_not_ours() {
    let rig = Rig::new("u8a-refusal-stale");
    rig.set_status(InstallerStatus {
        state: InstallerState::Refused,
        refusal: "hash: old".to_owned(),
        acked_order_id: 7,
        ..idle()
    });
    let (mut core, _) = started(&rig);
    assert_eq!(body(&mut core).phase, Some(Phase::Accepted));
}

#[test]
fn an_incompatible_staged_release_fails_the_update_and_drops_the_stage() {
    let rig = Rig::new("u8a-staged-incompat");
    let (mut core, _) = started(&rig);
    *rig.gate.staged.lock().unwrap() = Compat::Incompatible {
        rule: "R4".to_owned(),
    };
    rig.set_status(staged(B));
    let s = body(&mut core);
    assert_eq!(s.phase, Some(Phase::Failed));
    let f = s.failure.expect("failure");
    assert_eq!((f.step, f.reason.as_str()), (Step::Compat, "R4"));
    assert_eq!(rig.sup.starts(), 0);
    assert_eq!(written(&rig)[1], (Verb::Revert, B.to_owned()));
}

#[test]
fn an_incomplete_staged_release_fails_at_the_compat_step() {
    let rig = Rig::new("u8a-staged-incomplete");
    let (mut core, _) = started(&rig);
    *rig.gate.staged.lock().unwrap() = Compat::Incomplete {
        reason: "no probe".to_owned(),
    };
    rig.set_status(staged(B));
    let f = body(&mut core).failure.expect("failure");
    assert_eq!((f.step, f.reason.as_str()), (Step::Compat, "no probe"));
    assert_eq!(rig.sup.starts(), 0);
}

#[test]
fn a_supervisor_that_cannot_start_fails_the_update_and_drops_the_stage() {
    let rig = Rig::new("u8a-start-fails");
    let (mut core, _) = started(&rig);
    rig.sup.0.lock().unwrap().start_fails = true;
    rig.set_status(staged(B));
    let f = body(&mut core).failure.expect("failure");
    assert_eq!(f.step, Step::Start);
    assert_eq!(written(&rig)[1].0, Verb::Revert);
}

#[test]
fn abort_while_staging_drops_the_stage_and_frees_the_slot() {
    let rig = Rig::new("u8a-abort-staging");
    let (mut core, id) = started(&rig);
    let (aborted, phase) = accepted(core.abort());
    assert_eq!((aborted, phase), (id, Phase::RolledBack));
    assert_eq!(written(&rig)[1], (Verb::Revert, B.to_owned()));
    assert_eq!(body(&mut core).phase, Some(Phase::RolledBack));
    accepted(core.post(req(COMMIT_C, "r2")));
}

#[test]
fn abort_while_staging_with_an_unwritable_spool_keeps_the_update() {
    let rig = Rig::new("u8a-abort-nospool");
    let (mut core, _) = started(&rig);
    *rig.orders.fail.lock().unwrap() = true;
    assert_eq!(refused(core.abort()).code, "order_unwritable");
    assert_eq!(body(&mut core).phase, Some(Phase::Accepted));
}

#[test]
fn abort_with_nothing_open_or_past_the_flip_is_refused() {
    let rig = Rig::new("u8a-abort-refused");
    let mut core = rig.core();
    assert_eq!(refused(core.abort()).code, "no_update_open");
    for phase in [
        Phase::Switching,
        Phase::Switched,
        Phase::Baking,
        Phase::Draining,
    ] {
        rig.sup.set(phase, Some(old()), 0);
        let e = refused(core.abort());
        assert_eq!(
            (e.status, e.code),
            (StatusCode::CONFLICT, "not_abortable"),
            "{phase:?}"
        );
    }
    assert!(!rig.sup.calls().contains(&"abort"));
    for phase in [
        Phase::Preflight,
        Phase::Starting,
        Phase::Holding,
        Phase::Quiescing,
    ] {
        rig.sup.set(phase, None, 0);
        let (_, p) = accepted(core.abort());
        assert_eq!(p, Phase::RolledBack, "{phase:?}");
    }
}

#[test]
fn rollback_is_refused_before_the_flip_after_the_old_child_died_and_after_a_node_updated() {
    let rig = Rig::new("u8a-rollback");
    let mut core = rig.core();
    assert_eq!(refused(core.rollback()).code, "no_update_open");
    let cases = [
        (Phase::Staged, Some(old()), 0, "notswitched"),
        (Phase::Switching, Some(old()), 0, "notswitched"),
        (Phase::Switched, None, 0, "oldchildgone"),
        (Phase::Baking, Some(old()), 1, "nodesupdated"),
        (Phase::Nodes, Some(old()), 3, "nodesupdated"),
    ];
    for (phase, o, nodes, why) in cases {
        rig.sup.set(phase, o, nodes);
        let e = refused(core.rollback());
        assert_eq!(
            (e.status, e.code),
            (StatusCode::CONFLICT, "rollback_refused"),
            "{phase:?}"
        );
        assert_eq!(e.extra, Some(("reason", why.to_owned())), "{phase:?}");
    }
    assert!(
        !rig.sup.calls().contains(&"rollback"),
        "the supervisor was never asked"
    );
}

#[test]
fn rollback_while_staging_is_refused() {
    let rig = Rig::new("u8a-rollback-staging");
    let (mut core, _) = started(&rig);
    assert_eq!(
        refused(core.rollback()).extra,
        Some(("reason", "notswitched".to_owned()))
    );
}

#[test]
fn rollback_after_the_flip_with_the_old_child_alive_and_no_node_updated_goes_through() {
    let rig = Rig::new("u8a-rollback-ok");
    let mut core = rig.core();
    for phase in [
        Phase::Switched,
        Phase::Baking,
        Phase::Nodes,
        Phase::Draining,
    ] {
        rig.sup.set(phase, Some(old()), 0);
        let (id, p) = accepted(core.rollback());
        assert_eq!(
            (id.0.as_str(), p),
            ("upd-9", Phase::RolledBack),
            "{phase:?}"
        );
    }
}

#[test]
fn force_retire_needs_a_reason_and_an_old_child() {
    let rig = Rig::new("u8a-retire");
    let mut core = rig.core();
    for reason in ["", "   ", "\n"] {
        assert_eq!(refused(core.force_retire(reason)).code, "reason_required");
    }
    assert_eq!(
        refused(core.force_retire(&"x".repeat(513))).code,
        "reason_too_long"
    );
    assert_eq!(
        refused(core.force_retire("stuck stream")).code,
        "no_old_child"
    );
    assert!(rig.sup.calls().is_empty());
    rig.sup.set(Phase::Draining, Some(old()), 2);
    match core.force_retire("  a hung client  ") {
        Ok(Reply::Retired {
            update_id,
            dropped_streams,
        }) => {
            assert_eq!(update_id.map(|u| u.0), Some("upd-9".to_owned()));
            assert_eq!(dropped_streams, 4);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.sup.calls(), vec!["force_retire"]);
}

#[test]
fn force_retire_works_on_an_old_child_left_by_an_ended_update() {
    let rig = Rig::new("u8a-retire-left");
    rig.sup.0.lock().unwrap().status.old_child_draining = true;
    assert!(matches!(
        rig.core().force_retire("host is being replaced"),
        Ok(Reply::Retired {
            update_id: None,
            ..
        })
    ));
}

#[test]
fn status_reports_the_supervisors_update_and_the_gate_on_rollback() {
    let rig = Rig::new("u8a-status");
    let mut core = rig.core();
    let idle_body = body(&mut core);
    assert_eq!(idle_body.update_id, None);
    assert_eq!(
        idle_body.rollback_blocked_reason,
        Some(RollbackBlocked::NoUpdate)
    );
    assert!(!idle_body.restart_pending);

    rig.sup.set(Phase::Baking, Some(old()), 0);
    std::fs::write(rig.marker_path(), b"").unwrap();
    let s = body(&mut core);
    assert_eq!(s.update_id.map(|u| u.0), Some("upd-9".to_owned()));
    assert_eq!((s.from, s.to), (Some(c(A)), Some(c(B))));
    assert_eq!(s.phase, Some(Phase::Baking));
    assert_eq!(s.since, Some(1_800_000_000));
    assert!(s.restart_pending, "the postinst marker is set");
    assert!(s.rollback_allowed);
    assert_eq!(s.rollback_blocked_reason, None);
    assert_eq!(s.old, Some(old()));
    assert_eq!(s.new.map(|n| n.open_streams), Some(3));
    assert_eq!(s.failure, None);

    rig.sup.set(Phase::Baking, Some(old()), 2);
    let s = body(&mut core);
    assert!(!s.rollback_allowed);
    assert_eq!(
        s.rollback_blocked_reason,
        Some(RollbackBlocked::NodesUpdated)
    );

    rig.sup.set(Phase::Done, None, 2);
    assert_eq!(
        body(&mut core).rollback_blocked_reason,
        Some(RollbackBlocked::NoUpdate)
    );
}

#[test]
fn status_json_written_by_the_installer_parses() {
    use crate::helper::memory::{Event, Memory, StagedFacts};
    use crate::helper::status::render;
    use crate::state::State;
    let rig = Rig::new("u8a-contract");
    let state = State {
        current: Some(c(A)),
        staged: Some(c(B)),
        ..State::default()
    };
    let memory = Memory {
        acked_order_id: 12,
        event: Event::Refused,
        refusal: "hash: the digest differs".to_owned(),
        staged: Some(StagedFacts {
            commit: c(B),
            protocol: 5,
            min_protocol: 3,
            state_schema: 7,
        }),
        ..Memory::default()
    };
    let path = rig.dir.join("status.json");
    std::fs::write(&path, render(&state, &memory)).unwrap();
    let read = JsonFileStatus::new(&path).read().expect("parses");
    assert_eq!(read.installed_commit, Some(c(A)));
    assert_eq!(read.staged_commit, Some(c(B)));
    assert_eq!(read.state, InstallerState::Refused);
    assert_eq!((read.refusal_word(), read.acked_order_id), ("hash", 12));
    assert_eq!(
        read.staged_facts()
            .map(|f| (f.commit, f.protocol, f.min_protocol, f.state_schema)),
        Some((c(B), 5, 3, 7))
    );
    assert_eq!(idle().staged_facts(), None, "nothing staged, no facts");
    assert!(
        JsonFileStatus::new(rig.dir.join("none.json"))
            .read()
            .is_err()
    );
    std::fs::write(&path, b"{").unwrap();
    assert!(JsonFileStatus::new(&path).read().is_err());
}

#[test]
fn an_unknown_state_word_is_busy_not_idle() {
    let s: InstallerStatus =
        serde_json::from_str(r#"{"state":"quarantined","new_field":1}"#).unwrap();
    assert_eq!(s.state, InstallerState::Unknown);
    assert_eq!(InstallerStatus::default().refusal_word(), "");
}

#[test]
fn phases_order_and_the_abort_boundary() {
    assert!(abortable(Phase::Quiescing) && !abortable(Phase::Switching));
    assert!(!abortable(Phase::Done) && !abortable(Phase::Failed));
    assert!(!Phase::Draining.is_terminal() && Phase::Done.is_terminal());
    assert_eq!(
        serde_json::to_string(&Phase::RolledBack).unwrap(),
        "\"rolled_back\""
    );
}

#[test]
fn a_request_this_process_no_longer_tracks_is_retired_not_restarted() {
    let rig = Rig::new("u8a-retired");
    let (id, _) = accepted(rig.core().post(req(B, "r1")));
    // A restarted process: the request is on disk as started, the update is nowhere.
    let e = refused(rig.core().post(req(B, "r1")));
    assert_eq!(
        (e.status, e.code),
        (StatusCode::CONFLICT, "request_id_retired")
    );
    assert_eq!(id.0, "upd-1");
    assert_eq!(written(&rig).len(), 1);
}

#[test]
fn a_retried_request_finds_its_update_ended_or_with_the_supervisor() {
    let rig = Rig::new("u8a-replay-phases");
    let (mut core, id) = started(&rig);
    // Handed to the supervisor: the retry reports the supervisor's phase.
    rig.set_status(staged(B));
    let (again, phase) = accepted(core.post(req(B, "r1")));
    assert_eq!((again, phase), (id.clone(), Phase::Preflight));
    // Ended inside the API: the retry reports how it ended.
    let rig2 = Rig::new("u8a-replay-ended");
    let (mut core2, id2) = started(&rig2);
    accepted(core2.abort());
    let (again, phase) = accepted(core2.post(req(B, "r1")));
    assert_eq!((again, phase), (id2, Phase::RolledBack));
}

#[test]
fn a_started_mark_that_cannot_be_saved_does_not_undo_the_update() {
    struct MovesTheStateAway(std::path::PathBuf);
    impl CrashHook for MovesTheStateAway {
        fn at(&mut self, _point: CrashPoint) -> Result<(), Crash> {
            std::fs::rename(&self.0, self.0.with_extension("moved")).unwrap();
            Ok(())
        }
    }
    let rig = Rig::new("u8a-nomark");
    let state = rig.dir.join("state");
    std::fs::create_dir(&state).unwrap();
    let mut core = {
        let mut rig = Rig::new("u8a-nomark-parts");
        rig.dir = state.clone();
        rig.core()
    };
    core.set_hook(Box::new(MovesTheStateAway(state)));
    let (_, phase) = accepted(core.post(req(B, "r1")));
    assert_eq!(phase, Phase::Accepted);
}

#[test]
fn a_revert_order_that_cannot_be_written_still_ends_the_update() {
    let rig = Rig::new("u8a-revert-fails");
    let (mut core, _) = started(&rig);
    *rig.gate.staged.lock().unwrap() = Compat::Incompatible {
        rule: "R1".to_owned(),
    };
    *rig.orders.fail.lock().unwrap() = true;
    rig.set_status(staged(B));
    assert_eq!(body(&mut core).phase, Some(Phase::Failed));
    assert_eq!(written(&rig).len(), 1);
}

#[test]
fn supervisor_errors_become_defined_answers() {
    let rig = Rig::new("u8a-sup-errors");
    let mut core = rig.core();
    let cases = [
        (
            SupervisorError::NotOpen,
            StatusCode::CONFLICT,
            "no_update_open",
        ),
        (
            SupervisorError::Refused("busy".to_owned()),
            StatusCode::CONFLICT,
            "refused",
        ),
        (
            SupervisorError::Failed("boom".to_owned()),
            StatusCode::INTERNAL_SERVER_ERROR,
            "supervisor_failed",
        ),
    ];
    for (err, status, code) in cases {
        let again = err.clone();
        rig.sup.set(Phase::Preflight, None, 0);
        rig.sup.0.lock().unwrap().fail = Some(err);
        let e = refused(core.abort());
        assert_eq!((e.status, e.code), (status, code));
        rig.sup.set(Phase::Baking, Some(old()), 0);
        rig.sup.0.lock().unwrap().fail = Some(again.clone());
        let e = refused(core.rollback());
        assert_eq!((e.status, e.code), (status, code));
        rig.sup.set(Phase::Draining, Some(old()), 0);
        rig.sup.0.lock().unwrap().fail = Some(again);
        let e = refused(core.force_retire("why"));
        assert_eq!((e.status, e.code), (status, code));
    }
}

#[test]
fn small_pieces_say_what_they_say() {
    assert_eq!(NoCrash.at(CrashPoint::AfterRequestSaved), Ok(()));
    assert_eq!(StubGate.check(&c(A), None), Compat::Compatible);
    assert_eq!(StubGate.check(&c(A), Some(&facts())), Compat::Compatible);
    for (why, text) in [
        (RollbackBlocked::NoUpdate, "no update is open"),
        (
            RollbackBlocked::NotSwitched,
            "the update has not switched yet (use abort)",
        ),
        (RollbackBlocked::OldChildGone, "the old child is gone"),
        (
            RollbackBlocked::NodesUpdated,
            "a node has already been updated",
        ),
    ] {
        assert_eq!(why.describe(), text);
    }
}

#[test]
fn a_lost_start_is_revived_once_the_other_update_has_ended() {
    let rig = Rig::new("u8a-revive");
    *rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
    assert_eq!(rig.core().post(req(B, "r1")), Err(Fault::Crash));
    let mut restarted = rig.core();
    accepted(restarted.post(req(COMMIT_C, "r2")));
    accepted(restarted.abort());
    let (id, phase) = accepted(restarted.post(req(B, "r1")));
    assert_eq!((id.0.as_str(), phase), ("upd-1", Phase::Accepted));
}

#[test]
fn since_moves_with_each_phase_change_the_api_owns() {
    let rig = Rig::new("u8a-since");
    let (mut core, _) = started(&rig);
    let accepted_at = body(&mut core).since.expect("a since");
    // Nothing changed: reading again keeps the time the phase began.
    rig.clock.0.store(5_000, Ordering::SeqCst);
    assert_eq!(body(&mut core).since, Some(accepted_at));
    // accepted -> fetching
    rig.set_status(InstallerStatus {
        state: InstallerState::Fetching,
        ..idle()
    });
    let s = body(&mut core);
    assert_eq!(s.phase, Some(Phase::Fetching));
    assert_eq!(s.since, Some(accepted_at + 5));
    // Still fetching: the time stays.
    rig.clock.0.store(9_000, Ordering::SeqCst);
    assert_eq!(body(&mut core).since, Some(accepted_at + 5));
    // fetching -> failed (the installer refused our order)
    rig.set_status(InstallerStatus {
        state: InstallerState::Refused,
        refusal: "disk_full: no room".to_owned(),
        acked_order_id: 101,
        ..idle()
    });
    let s = body(&mut core);
    assert_eq!(s.phase, Some(Phase::Failed));
    assert_eq!(s.since, Some(accepted_at + 9));
}

#[test]
fn a_revived_request_is_checked_again_like_a_new_one() {
    type Break = fn(&Rig);
    let cases: [(&str, Break, &str); 4] = [
        (
            "same",
            |r| r.sup.0.lock().unwrap().status.running = Some(c(B)),
            "same_commit",
        ),
        (
            "compat",
            |r| {
                *r.gate.before.lock().unwrap() = Compat::Incompatible {
                    rule: "R1".to_owned(),
                }
            },
            "incompatible",
        ),
        (
            "busy",
            |r| {
                r.set_status(InstallerStatus {
                    state: InstallerState::Fetching,
                    ..idle()
                })
            },
            "installer_busy",
        ),
        (
            "draining",
            |r| r.sup.0.lock().unwrap().status.old_child_draining = true,
            "old_child_draining",
        ),
    ];
    for (name, brk, code) in cases {
        let rig = Rig::new(&format!("u8a-revive-check-{name}"));
        *rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
        assert_eq!(rig.core().post(req(B, "r1")), Err(Fault::Crash), "{name}");
        brk(&rig);
        let e = refused(rig.core().post(req(B, "r1")));
        assert_eq!(e.code, code, "{name}");
        assert!(
            written(&rig).is_empty(),
            "{name}: no order for a refused revival"
        );
    }
}
