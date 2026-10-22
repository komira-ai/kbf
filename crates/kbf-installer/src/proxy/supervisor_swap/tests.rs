//! The supervisor over a thread-safe world ([`super::rig::Rig`]): the swap runs on its own
//! thread in real time, with limits of milliseconds, and each test watches it from here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::rig::{Rig, quick};
use super::status::active_after;
use super::{Parts, Spawn, SwapSupervisor, panic_text, spawn_error};
use crate::proxy::kinds::{OldChild, Retired, Supervisor, SupervisorError, UpdateId, UpdateSpec};
use crate::proxy::model::{
    AdoptError, Failure, Intent, Locator, Mode, Phase, Plan, ProcIdent, Request, Slot, StagedFacts,
    Step, Timing, Verb,
};
use crate::proxy::ports::{Child, ChildAdmin, Gate, Recorder, RootOrders};
use crate::proxy::runtime::RequestQueue;
use crate::testkit::{A, B, C, c};

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    rig: Arc<Rig>,
    requests: Arc<RequestQueue>,
    dir: PathBuf,
    deaths: Arc<Mutex<Vec<String>>>,
    sup: SwapSupervisor,
}

fn build(
    rig: &Arc<Rig>,
    requests: &Arc<RequestQueue>,
    dir: &Path,
    deaths: &Arc<Mutex<Vec<String>>>,
    timing: Timing,
    rollback: bool,
) -> SwapSupervisor {
    let sink = Arc::clone(deaths);
    let parts: Parts = rig.parts(requests, move |why| {
        sink.lock().unwrap().push(why.to_owned())
    });
    let mut config = Rig::config(dir.to_path_buf());
    config.timing = timing;
    config.rollback_after_switch = rollback;
    SwapSupervisor::new(parts, config)
}

fn harness(name: &str) -> Harness {
    harness_with(name, quick(), false)
}

fn harness_with(name: &str, timing: Timing, rollback: bool) -> Harness {
    custom(name, timing, rollback, |_| {})
}

/// A harness whose parts `tweak` changes before the supervisor is built.
fn custom(name: &str, timing: Timing, rollback: bool, tweak: impl FnOnce(&mut Parts)) -> Harness {
    let dir = kbf_host::testkit::scratch(name);
    let rig = Rig::new(Some(dir.clone()));
    let requests = Arc::new(RequestQueue::new());
    let deaths = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&deaths);
    let mut parts = rig.parts(&requests, move |why| {
        sink.lock().unwrap().push(why.to_owned());
    });
    tweak(&mut parts);
    let mut config = Rig::config(dir.to_path_buf());
    config.timing = timing;
    config.rollback_after_switch = rollback;
    Harness {
        rig,
        requests,
        dir,
        deaths,
        sup: SwapSupervisor::new(parts, config),
    }
}

/// A spawner that cannot start a thread.
fn no_threads() -> Spawn {
    Box::new(|_| Err("no threads left".into()))
}

fn spec(update_id: &str, from: Option<&str>, to: &str) -> UpdateSpec {
    UpdateSpec {
        update_id: UpdateId(update_id.to_owned()),
        request_id: format!("req-{update_id}"),
        from: from.map(c),
        to: c(to),
        nodes: true,
        rollback: false,
        source: kbf_update::Source::Fetch,
        facts: StagedFacts {
            commit: c(to),
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: None,
        },
        order: Locator {
            order_id: 101,
            epoch: 1,
            generation: 1,
            verb: Verb::FrontendUpdate,
            commit: c(to),
            window_s: 900,
            source: kbf_update::Source::Fetch,
        },
    }
}

/// Wait until `f` holds, or fail naming `what`.
fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn phase_of(sup: &SwapSupervisor) -> Option<Phase> {
    sup.status().update.map(|u| u.phase)
}

fn last_intent(rig: &Rig) -> Intent {
    rig.st().records.last().cloned().expect("a record")
}

/// The log lines named by `prefixes` appear in this order.
fn in_order(rig: &Rig, prefixes: &[&str]) {
    let mut at = 0;
    for p in prefixes {
        let found = rig
            .log()
            .iter()
            .enumerate()
            .skip(at)
            .find(|(_, l)| l.starts_with(p))
            .map(|(i, _)| i);
        match found {
            Some(i) => at = i + 1,
            None => panic!("{p:?} not found after position {at} in {:#?}", rig.log()),
        }
    }
}

fn failure(rig: &Rig) -> Failure {
    last_intent(rig).failure.expect("the update failed")
}

#[test]
fn an_update_goes_through_every_phase_in_the_order_of_the_design() {
    let mut h = harness("sw-happy");
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    let mut phases = h.rig.phases();
    phases.dedup();
    assert_eq!(
        phases,
        [
            Phase::Staged,
            Phase::Preflight,
            Phase::Starting,
            Phase::Ready,
            Phase::Holding,
            Phase::Quiescing,
            Phase::Switching,
            Phase::Switched,
            Phase::Baking,
            Phase::Draining,
            Phase::Done,
        ]
    );
    in_order(
        &h.rig,
        &[
            "migrate",
            "spawn B Candidate",
            "hold",
            "admin:quiesce A",
            "admin:takeover B",
            "release_into B",
            "admin:stand_down A",
            "order confirm",
            "terminate pid 100",
        ],
    );
    let status = h.sup.status();
    let update = status.update.expect("the update");
    assert_eq!((update.phase, update.active_slot), (Phase::Done, Slot::B));
    // The spec asks for the nodes (`nodes: true`) and the swap does nothing for them.
    assert_eq!(update.nodes_updated, 0);
    assert_eq!(status.running, Some(c(B)));
    assert!(update.old.is_none(), "nothing drains after the end");
    assert!(h.deaths.lock().unwrap().is_empty());
}

/// Catches a source that stops at the supervisor: the update it was given is `staged`, so the
/// record in `update.json` says so (a restarted proxy has no other source to read), and the
/// status reads it from that record.
///
/// Mutants planted: the plan built with source `fetch`; the status always `fetch`.
#[test]
fn the_source_of_an_update_is_kept_in_its_record() {
    let mut h = harness("sw-source");
    let staged = UpdateSpec {
        source: kbf_update::Source::Staged,
        ..spec("u1", Some(A), B)
    };
    h.sup.start(&staged).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(last_intent(&h.rig).source, kbf_update::Source::Staged);
    let update = h.sup.status().update.expect("the update");
    assert_eq!(update.source, kbf_update::Source::Staged);

    let mut h = harness("sw-source-fetch");
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(last_intent(&h.rig).source, kbf_update::Source::Fetch);
    let update = h.sup.status().update.expect("the update");
    assert_eq!(update.source, kbf_update::Source::Fetch);
}

#[test]
fn start_is_idempotent_on_the_update_id_and_refuses_a_second_open_update() {
    let mut h = harness("sw-idem");
    h.rig.knobs(|k| k.ready = false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    let other = h.sup.start(&spec("u2", Some(A), C));
    assert!(
        matches!(&other, Err(SupervisorError::Refused(m)) if m.contains("u1")),
        "{other:?}"
    );
    until("the candidate", || h.rig.count("spawn B") == 1);
    h.rig.knobs(|k| k.ready = true);
    assert!(h.sup.wait_closed(WAIT));
    // The update has ended; the same id is still the same update.
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert_eq!(h.rig.count("spawn B"), 1, "one swap, one candidate");
    assert_eq!(h.rig.count("migrate"), 1);
    assert_eq!(phase_of(&h.sup), Some(Phase::Done));
}

#[test]
fn start_without_the_running_commit_is_refused_and_starts_nothing() {
    let mut h = harness("sw-nofrom");
    let r = h.sup.start(&spec("u1", None, B));
    assert!(matches!(r, Err(SupervisorError::Refused(_))), "{r:?}");
    assert!(h.rig.log().is_empty());
    assert_eq!(phase_of(&h.sup), None);
}

#[test]
fn abort_before_the_switch_ends_the_update_with_the_old_child_untouched() {
    let mut h = harness("sw-abort");
    h.rig.knobs(|k| k.ready = false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("the candidate is starting", || {
        phase_of(&h.sup) == Some(Phase::Starting) && h.rig.count("spawn B") == 1
    });
    let answered = h.sup.abort().unwrap();
    assert_eq!(answered, Phase::Starting);
    assert!(h.sup.wait_closed(WAIT));
    let f = failure(&h.rig);
    assert_eq!(f.step, Step::Abort);
    assert_eq!(last_intent(&h.rig).phase, Phase::Failed);
    assert_eq!(h.rig.count("kill pid 101"), 1, "the candidate never served");
    assert_eq!(h.rig.count("hold"), 0, "no stream was ever held");
    assert!(h.rig.alive(Slot::A));
    assert_eq!(h.rig.count("admin:quiesce"), 0);
    assert!(
        h.rig.pos("order revert").is_some(),
        "the staged release is dropped"
    );
}

#[test]
fn abort_and_rollback_with_no_update_open_say_so() {
    let mut h = harness("sw-notopen");
    assert_eq!(h.sup.abort(), Err(SupervisorError::NotOpen));
    assert_eq!(h.sup.rollback(), Err(SupervisorError::NotOpen));
    assert_eq!(h.sup.force_retire("x"), Err(SupervisorError::NotOpen));
}

#[test]
fn rollback_is_refused_while_the_old_child_cannot_resume_after_a_stand_down() {
    let mut h = harness("sw-rbrefused");
    h.rig.knobs(|k| k.ready = false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("the candidate", || h.rig.count("spawn B") == 1);
    let r = h.sup.rollback();
    assert!(
        matches!(&r, Err(SupervisorError::Refused(m)) if m.contains("not supported")),
        "{r:?}"
    );
    h.sup.abort().unwrap();
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(
        failure(&h.rig).step,
        Step::Abort,
        "the refusal queued nothing"
    );
}

#[test]
fn a_migrator_refusal_starts_no_candidate_and_leaves_the_old_child_serving() {
    let mut h = harness("sw-migrate");
    h.rig
        .knobs(|k| k.migrate_err = Some("not_additive: the release needs schema 4".into()));
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    let f = failure(&h.rig);
    assert_eq!(f.step, Step::Migrate);
    assert!(f.reason.starts_with("not_additive"), "{}", f.reason);
    assert_eq!(h.rig.count("spawn"), 0);
    assert_eq!(h.rig.count("hold"), 0);
    assert!(h.rig.pos("order revert").is_some());
    let status = h.sup.status();
    assert_eq!(status.running, Some(c(A)));
    assert_eq!(status.update.expect("update").active_slot, Slot::A);
}

#[test]
fn a_failed_takeover_puts_the_old_child_back_and_the_update_fails_at_switch() {
    let mut h = harness("sw-takeover");
    h.rig.knobs(|k| k.takeover_err = true);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(failure(&h.rig).step, Step::Switch);
    in_order(
        &h.rig,
        &[
            "hold",
            "admin:quiesce A",
            "admin:takeover B",
            "kill pid 101",
            "admin:resume A",
            "release_into A",
        ],
    );
    assert!(h.rig.alive(Slot::A) && !h.rig.alive(Slot::B));
    assert_eq!(
        h.rig.count("release_into B"),
        0,
        "no stream went to the candidate"
    );
    let status = h.sup.status();
    assert_eq!(status.running, Some(c(A)));
    assert_eq!(status.update.expect("update").active_slot, Slot::A);
}

#[test]
fn a_swap_thread_that_panics_runs_on_death_and_closes_the_update() {
    let mut h = harness("sw-panic");
    h.rig.knobs(|k| k.spawn_panics = true);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(
        h.sup.wait_closed(WAIT),
        "the update closes when the thread dies"
    );
    let deaths = h.deaths.lock().unwrap().clone();
    assert_eq!(deaths.len(), 1, "{deaths:?}");
    assert!(
        deaths[0].contains("panicked") && deaths[0].contains("spawn exploded"),
        "{deaths:?}"
    );
}

#[test]
fn a_shutdown_ends_the_swap_without_calling_on_death() {
    let mut h = harness("sw-shutdown");
    h.rig.knobs(|k| k.ready = false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("the candidate", || h.rig.count("spawn B") == 1);
    h.rig.stop(true);
    assert!(h.sup.wait_closed(WAIT));
    assert!(h.deaths.lock().unwrap().is_empty());
    // update.json says where it stopped, for the next start.
    assert_eq!(last_intent(&h.rig).phase, Phase::Starting);
}

#[test]
fn the_status_shows_the_old_child_while_it_drains_and_force_retire_ends_it() {
    let mut h = harness("sw-drain");
    h.rig.knobs(|k| k.streams = [3, 0]);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("draining", || phase_of(&h.sup) == Some(Phase::Draining));
    let status = h.sup.status();
    let update = status.update.expect("update");
    assert_eq!(update.active_slot, Slot::B);
    assert_eq!(update.nodes_updated, 0);
    assert_eq!(
        update.old,
        Some(OldChild {
            open_streams: 3,
            oldest_stream_age_s: 0,
            open_executions: 3,
            queued_operations: 0,
        })
    );
    assert_eq!(
        h.rig.count("terminate"),
        0,
        "streams are open: nothing is signalled"
    );
    let retired = h.sup.force_retire("a stuck stream").unwrap();
    assert_eq!(retired, Retired { dropped_streams: 3 });
    assert_eq!(h.rig.count("terminate pid 100"), 1);
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(phase_of(&h.sup), Some(Phase::Done));
}

#[test]
fn rollback_after_the_switch_restores_the_old_child_when_enabled() {
    let timing = Timing {
        bake: Duration::from_secs(30),
        ..quick()
    };
    let mut h = harness_with("sw-rollback", timing, true);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("baking", || phase_of(&h.sup) == Some(Phase::Baking));
    assert_eq!(h.sup.rollback().unwrap(), Phase::Baking);
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(last_intent(&h.rig).phase, Phase::RolledBack);
    in_order(
        &h.rig,
        &[
            "admin:stand_down B",
            "admin:resume A",
            "release_into A",
            "order revert",
            "terminate pid 101",
        ],
    );
    assert_eq!(h.sup.status().running, Some(c(A)));
    assert_eq!(h.rig.count("order confirm"), 0, "nothing was confirmed");
}

#[test]
fn a_restarted_supervisor_resumes_an_update_from_update_json_and_spawns_nothing_again() {
    let timing = Timing {
        bake: Duration::from_secs(30),
        ..quick()
    };
    let mut h = harness_with("sw-restart", timing, false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("baking", || phase_of(&h.sup) == Some(Phase::Baking));
    // The proxy dies here: its swap thread stops, the children live on.
    h.rig.stop(true);
    assert!(h.sup.wait_closed(WAIT));
    drop(h.sup);
    assert_eq!(
        record_on_disk(&h.dir).phase,
        Phase::Baking,
        "update.json is where the swap stopped"
    );
    h.rig.stop(false);
    let mut again = build(&h.rig, &h.requests, &h.dir, &h.deaths, quick(), false);
    assert!(again.wait_closed(WAIT), "the resumed swap ends");
    assert_eq!(phase_of(&again), Some(Phase::Done));
    assert_eq!(
        h.rig.count("spawn"),
        1,
        "the candidate is adopted, not spawned again"
    );
    assert_eq!(h.rig.count("order confirm"), 1);
    // The same update id, offered again by the API's retry, is the same update.
    again.start(&spec("u1", Some(A), B)).unwrap();
    assert_eq!(h.rig.count("spawn"), 1);
}

#[test]
fn after_a_restart_the_next_update_goes_from_the_slot_the_last_one_ended_in() {
    let mut h = harness("sw-slots");
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    drop(h.sup);
    let mut again = build(&h.rig, &h.requests, &h.dir, &h.deaths, quick(), false);
    let status = again.status();
    assert_eq!(status.running, Some(c(B)), "the record says what runs");
    assert_eq!(status.update.expect("update").active_slot, Slot::B);
    h.rig.knobs(|k| k.staged = Some(c(C)));
    again.start(&spec("u2", Some(B), C)).unwrap();
    assert!(again.wait_closed(WAIT));
    assert_eq!(
        h.rig.count("spawn A Candidate"),
        1,
        "the idle slot is A now"
    );
    assert_eq!(last_intent(&h.rig).phase, Phase::Done);
    assert_eq!(again.status().update.expect("update").active_slot, Slot::A);
}

#[test]
fn an_unreadable_update_json_refuses_every_start_and_touches_no_child() {
    let dir = kbf_host::testkit::scratch("sw-corrupt");
    std::fs::write(dir.join("update.json"), b"{ not json").unwrap();
    let rig = Rig::new(None);
    let requests = Arc::new(RequestQueue::new());
    let deaths = Arc::new(Mutex::new(Vec::new()));
    let mut sup = build(&rig, &requests, &dir, &deaths, quick(), false);
    let r = sup.start(&spec("u1", Some(A), B));
    assert!(
        matches!(&r, Err(SupervisorError::Refused(m)) if m.contains("unreadable")),
        "{r:?}"
    );
    assert!(rig.log().is_empty());
}

fn record_on_disk(dir: &Path) -> Intent {
    crate::proxy::record::FileRecorder::load(dir)
        .expect("readable")
        .expect("present")
}

fn intent(phase: Phase, failure: Option<Step>) -> Intent {
    let plan = Plan {
        update_id: "u".into(),
        request_id: "r".into(),
        mode: Mode::Frontend,
        from: c(A),
        to: c(B),
        nodes: true,
        rollback: false,
        source: kbf_update::Source::Fetch,
        old_slot: Slot::A,
        timing: quick(),
    };
    let mut i = Intent::begin(&plan, None, 0);
    i.phase = phase;
    i.failure = failure.map(|step| Failure {
        step,
        reason: "x".into(),
    });
    i
}

#[test]
fn the_slot_that_serves_after_an_update_follows_how_it_ended() {
    let new = Slot::B;
    let old = Slot::A;
    let never = || -> bool { panic!("the store is not asked") };
    assert_eq!(active_after(&intent(Phase::Done, None), &never), new);
    assert_eq!(active_after(&intent(Phase::RolledBack, None), &never), old);
    for (step, serves) in [
        (Step::Confirm, new),
        (Step::Rollback, new),
        // An older build's record can say it failed at the order for the nodes, which was
        // sent after the switch and the confirm: the new child serves.
        (Step::Nodes, new),
        (Step::Switch, old),
        (Step::Migrate, old),
        (Step::Abort, old),
        (Step::Start, old),
    ] {
        assert_eq!(
            active_after(&intent(Phase::Failed, Some(step)), &never),
            serves,
            "failed at {step:?}"
        );
    }
    let restart = intent(Phase::Failed, Some(Step::Restart));
    assert_eq!(
        active_after(&restart, &|| true),
        new,
        "the store says it holds"
    );
    assert_eq!(active_after(&restart, &|| false), old);
    assert_eq!(active_after(&intent(Phase::Switched, None), &never), new);
    assert_eq!(active_after(&intent(Phase::Quiescing, None), &never), old);
}

/// The window between the swap thread recording its last phase and closing the update: the
/// update still reads as open, its record already ended. The status names the slot that serves
/// once the update ended that way, not the one a live update in a later phase would name
/// (`failed` and `rolled_back` sort after `switched`).
#[test]
fn the_status_of_an_update_that_ended_but_is_not_closed_names_the_slot_that_serves() {
    let (old, new) = (Slot::A, Slot::B);
    for (phase, step, serves) in [
        (Phase::Failed, Some(Step::Abort), old),
        (Phase::Failed, Some(Step::Start), old),
        (Phase::Failed, Some(Step::Migrate), old),
        (Phase::Failed, Some(Step::Confirm), new),
        // No store answer for the new slot: it does not hold.
        (Phase::Failed, Some(Step::Restart), old),
        (Phase::RolledBack, None, old),
        (Phase::Done, None, new),
    ] {
        let h = harness("sw-status-ended");
        {
            let mut state = h.sup.shared.lock();
            state.intent = Some(intent(phase, step));
            state.open = Some("u".into());
        }
        let update = h.sup.status().update.expect("the update");
        assert_eq!(update.active_slot, serves, "{phase:?} {step:?}");
    }
}

fn rig_config(h: &Harness, timing: Timing, spawn: Spawn) -> SwapSupervisor {
    let sink = Arc::clone(&h.deaths);
    let parts = h.rig.parts(&h.requests, move |why| {
        sink.lock().unwrap().push(why.to_owned());
    });
    let mut config = Rig::config(h.dir.to_path_buf());
    config.timing = timing;
    SwapSupervisor::with_spawner(parts, config, spawn)
}

#[test]
fn a_swap_thread_that_cannot_start_ends_the_update_failed_at_start() {
    let mut h = harness("sw-nothread");
    h.sup = rig_config(&h, quick(), no_threads());
    let r = h.sup.start(&spec("u1", Some(A), B));
    assert!(
        matches!(&r, Err(SupervisorError::Failed(m)) if m.contains("no threads")),
        "{r:?}"
    );
    let update = h.sup.status().update.expect("the update is shown");
    assert_eq!(update.phase, Phase::Failed);
    assert_eq!(update.failure.map(|f| f.step), Some(Step::Start));
    assert_eq!(record_on_disk(&h.dir).phase, Phase::Failed);
    assert!(h.sup.wait_closed(WAIT), "nothing is left open");
    assert_eq!(h.rig.count("spawn"), 0);
}

#[test]
fn the_status_never_runs_ahead_of_update_json() {
    let mut h = harness("sw-nothread-record");
    h.sup = rig_config(&h, quick(), no_threads());
    h.rig.knobs(|k| k.fail_record = Some(Phase::Failed));
    assert!(h.sup.start(&spec("u1", Some(A), B)).is_err());
    // The record of the failure was refused, so it is not shown.
    assert_eq!(phase_of(&h.sup), Some(Phase::Staged));
    assert_eq!(record_on_disk(&h.dir).phase, Phase::Staged);
}

#[test]
fn a_record_that_cannot_be_written_refuses_the_start_before_anything_runs() {
    let mut h = harness("sw-norecord");
    h.rig.knobs(|k| k.fail_record = Some(Phase::Staged));
    let r = h.sup.start(&spec("u1", Some(A), B));
    assert!(
        matches!(&r, Err(SupervisorError::Failed(m)) if m.contains("refused")),
        "{r:?}"
    );
    assert!(h.rig.log().is_empty());
    assert_eq!(phase_of(&h.sup), None);
}

#[test]
fn a_port_the_swap_cannot_do_without_runs_on_death() {
    let mut h = harness("sw-halt");
    h.rig.knobs(|k| k.fail_record = Some(Phase::Preflight));
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    let deaths = h.deaths.lock().unwrap().clone();
    assert_eq!(deaths.len(), 1, "{deaths:?}");
    assert!(deaths[0].starts_with("the swap stopped:"), "{deaths:?}");
}

#[test]
fn a_request_left_from_an_earlier_update_is_not_this_updates() {
    let mut h = harness("sw-stale");
    h.requests.push(Request::Abort);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(
        phase_of(&h.sup),
        Some(Phase::Done),
        "the stale abort did nothing"
    );
}

#[test]
fn a_panic_is_told_whatever_its_payload() {
    let text: Box<dyn std::any::Any + Send> = Box::new("a str");
    assert_eq!(panic_text(&*text), "a str");
    let text: Box<dyn std::any::Any + Send> = Box::new(String::from("a string"));
    assert_eq!(panic_text(&*text), "a string");
    let other: Box<dyn std::any::Any + Send> = Box::new(5_u32);
    assert_eq!(panic_text(&*other), "no message");
    assert!(spawn_error(std::io::Error::other("x")).contains("could not start"));
}

/// An update held in the bake, so that the old child is alive beside the serving one.
fn baking(name: &str, tweak: impl FnOnce(&mut Parts)) -> Harness {
    let timing = Timing {
        bake: Duration::from_secs(30),
        ..quick()
    };
    let mut h = custom(name, timing, false, tweak);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("baking", || phase_of(&h.sup) == Some(Phase::Baking));
    h
}

fn end_bake(h: &Harness) {
    h.rig.stop(true);
    assert!(h.sup.wait_closed(WAIT));
}

#[test]
fn force_retire_refuses_an_old_child_that_was_never_recorded() {
    let mut h = baking("sw-fr-unrecorded", |p| p.old_proc = Box::new(|_| None));
    let r = h.sup.force_retire("x");
    assert!(
        matches!(&r, Err(SupervisorError::Refused(m)) if m.contains("not recorded")),
        "{r:?}"
    );
    end_bake(&h);
}

#[test]
fn force_retire_refuses_an_old_child_that_is_gone() {
    let mut h = baking("sw-fr-gone", |_| {});
    h.rig.kill(Slot::A);
    let r = h.sup.force_retire("x");
    assert!(
        matches!(&r, Err(SupervisorError::Refused(m)) if m.contains("gone")),
        "{r:?}"
    );
    end_bake(&h);
}

#[test]
fn force_retire_says_so_when_the_signal_cannot_be_sent() {
    let mut h = baking("sw-fr-signal", |_| {});
    h.rig.knobs(|k| k.terminate_err = true);
    let r = h.sup.force_retire("x");
    assert!(
        matches!(&r, Err(SupervisorError::Failed(m)) if m.contains("SIGTERM")),
        "{r:?}"
    );
    end_bake(&h);
}

#[test]
fn force_retire_after_a_rollback_ends_the_new_child_and_the_status_shows_it() {
    let timing = Timing {
        bake: Duration::from_secs(30),
        ..quick()
    };
    let mut h = harness_with("sw-fr-rollback", timing, true);
    h.rig.knobs(|k| k.streams = [0, 2]);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("baking", || phase_of(&h.sup) == Some(Phase::Baking));
    h.sup.rollback().unwrap();
    until("draining the new child", || {
        phase_of(&h.sup) == Some(Phase::Draining)
    });
    let update = h.sup.status().update.expect("update");
    assert_eq!(update.active_slot, Slot::A, "the old child serves again");
    assert_eq!(update.old.map(|o| o.open_streams), Some(2));
    let retired = h.sup.force_retire("a stuck stream").unwrap();
    assert_eq!(retired.dropped_streams, 2);
    assert_eq!(h.rig.count("terminate pid 101"), 1);
    assert!(h.sup.wait_closed(WAIT));
    assert_eq!(last_intent(&h.rig).phase, Phase::RolledBack);
}

fn stopped_at_starting(name: &str) -> Harness {
    let mut h = harness(name);
    h.rig.knobs(|k| k.ready = false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("the candidate", || h.rig.count("spawn B") == 1);
    h.rig.stop(true);
    assert!(h.sup.wait_closed(WAIT));
    h
}

#[test]
fn a_restart_that_finds_the_candidate_dead_puts_the_old_child_back() {
    let h = stopped_at_starting("sw-restart-dead");
    h.rig.kill(Slot::B);
    h.rig.stop(false);
    let again = build(&h.rig, &h.requests, &h.dir, &h.deaths, quick(), false);
    assert!(again.wait_closed(WAIT));
    assert_eq!(failure(&h.rig).step, Step::Restart);
    let update = again.status().update.expect("update");
    assert_eq!((update.phase, update.active_slot), (Phase::Failed, Slot::A));
}

#[test]
fn a_restart_that_cannot_read_the_root_halfs_status_fails_at_restart_and_asks_the_store() {
    let timing = Timing {
        bake: Duration::from_secs(30),
        ..quick()
    };
    let mut h = harness_with("sw-restart-status", timing, false);
    h.sup.start(&spec("u1", Some(A), B)).unwrap();
    until("baking", || phase_of(&h.sup) == Some(Phase::Baking));
    end_bake(&h);
    h.rig.knobs(|k| k.status_err = true);
    h.rig.stop(false);
    let again = build(&h.rig, &h.requests, &h.dir, &h.deaths, quick(), false);
    assert!(again.wait_closed(WAIT));
    assert_eq!(failure(&h.rig).step, Step::Restart);
    // The new child holds the generation, so it is the one that serves.
    assert_eq!(again.status().update.expect("update").active_slot, Slot::B);
}

#[test]
fn the_rig_answers_for_a_slot_with_nothing_in_it_as_a_port_would() {
    let rig = Rig::new(None);
    let nothing = Slot::B;
    let limit = Duration::from_secs(1);
    assert!(rig.role(nothing).is_err());
    assert!(rig.takeover(nothing, limit).is_err());
    assert!(rig.quiesce(nothing).is_err());
    assert!(rig.stand_down(nothing).is_err());
    assert!(rig.resume(nothing).is_err());
    assert!(rig.open_counts(nothing).is_err());
    assert!(!rig.ready(nothing).expect("answers").all());
    let ident = ProcIdent {
        pid: 7,
        start_time: 7,
        exe: "/x".into(),
    };
    assert_eq!(rig.adopt(nothing, &ident), Err(AdoptError::Gone(7)));
    assert_eq!(rig.exited(&ident), Some(0));
    assert_eq!(rig.retire(Slot::A), crate::proxy::model::Retired::Current);
    assert_eq!(rig.retire(nothing), crate::proxy::model::Retired::Closed);
    rig.kill(nothing);
    rig.record(&intent(Phase::Staged, None))
        .expect("kept in memory");
    rig.clear_active();
    rig.frontend_update(&spec("u", None, B).order)
        .expect("logged");
    assert!(
        rig.ready(Slot::A).expect("answers").all(),
        "the old child is up"
    );
}

#[test]
fn a_restart_whose_swap_thread_cannot_start_runs_on_death() {
    let h = stopped_at_starting("sw-restart-nothread");
    h.rig.stop(false);
    let again = rig_config(&h, quick(), no_threads());
    let deaths = h.deaths.lock().unwrap().clone();
    assert_eq!(deaths.len(), 1, "{deaths:?}");
    assert!(deaths[0].contains("no threads"), "{deaths:?}");
    assert!(again.wait_closed(WAIT), "the update is not left open");
}
