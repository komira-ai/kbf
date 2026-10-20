//! `Supervisor::start_blocked` of the swap supervisor: what each refusal is made of, and that
//! a host with nothing wrong is not refused. A `POST` that is blocked here writes no order, so
//! each arm is a release that is not fetched for an update nothing can take.

use std::path::PathBuf;
use std::sync::Arc;

use super::rig::{Rig, quick};
use super::{Spawn, SwapSupervisor};
use crate::proxy::api::model::UpdateId;
use crate::proxy::api::traits::{Supervisor, UpdateSpec};
use crate::proxy::model::{Locator, Phase, Slot, StagedFacts, Verb};
use crate::proxy::runtime::RequestQueue;
use crate::testkit::{A, B, c};

/// A supervisor over the rig whose swap thread is accepted and never run, so an update that
/// starts stays open for the whole test.
fn parked() -> Spawn {
    Box::new(|_work| Ok(()))
}

fn over(name: &str, edit: impl FnOnce(&Rig, &mut super::Config)) -> (SwapSupervisor, PathBuf) {
    let dir = kbf_host::testkit::scratch(name);
    let rig = Rig::new(Some(dir.clone()));
    let requests = Arc::new(RequestQueue::new());
    let parts = rig.parts(&requests, |_| {});
    let mut config = Rig::config(dir.clone());
    config.timing = quick();
    edit(&rig, &mut config);
    (SwapSupervisor::with_spawner(parts, config, parked()), dir)
}

fn spec(id: &str) -> UpdateSpec {
    UpdateSpec {
        update_id: UpdateId(id.to_owned()),
        request_id: format!("req-{id}"),
        from: Some(c(A)),
        to: c(B),
        nodes: true,
        rollback: false,
        facts: StagedFacts {
            commit: c(B),
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: None,
        },
        order: Locator {
            order_id: 1,
            epoch: 1,
            generation: 1,
            verb: Verb::FrontendUpdate,
            commit: c(B),
            window_s: 900,
        },
    }
}

#[test]
fn a_host_with_an_idle_slot_a_releases_directory_and_no_update_is_not_blocked() {
    let (sup, _dir) = over("blk-none", |_, _| {});
    assert_eq!(sup.start_blocked(), None);
}

/// Catches a `503` where the API's `409 update_open` belongs: `deploy` reads the first as a
/// failure and the second as "busy, not now". An update still going on, with the slot of its
/// candidate taken and the releases directory gone, is not reported here.
#[test]
fn an_update_in_flight_is_left_to_the_apis_conflict() {
    let (mut sup, _dir) = over("blk-open", |_, _| {});
    sup.start(&spec("u1")).expect("accepted");
    assert_eq!(sup.start_blocked(), None);
    sup.config.releases_dir = sup.config.state_dir.join("gone");
    assert_eq!(sup.start_blocked(), None);
}

/// Catches a new update fetched and then refused: an update that has ended but whose swap
/// thread is still open (draining the old child) refuses the next `start`, so the `POST` is
/// refused first.
#[test]
fn an_update_that_has_ended_but_is_still_finishing_blocks() {
    let (mut sup, _dir) = over("blk-finishing", |_, _| {});
    sup.start(&spec("u1")).expect("accepted");
    sup.shared.lock().intent.as_mut().expect("the intent").phase = Phase::Done;
    let why = sup.start_blocked().expect("blocked");
    assert!(
        why.contains("update u1 has ended and is still finishing"),
        "{why}"
    );
    sup.shared.close();
    assert_eq!(
        sup.start_blocked(),
        None,
        "closed: the next update can start"
    );
}

#[test]
fn a_missing_releases_directory_blocks() {
    let (sup, dir) = over("blk-releases", |_, c| {
        c.releases_dir = c.state_dir.join("no-such-releases");
    });
    let why = sup.start_blocked().expect("blocked");
    assert!(why.contains("releases directory"), "{why}");
    assert!(why.contains("no-such-releases"), "{why}");
    drop(dir);
}

#[test]
fn a_releases_path_that_is_a_file_blocks() {
    let (sup, _dir) = over("blk-releases-file", |_, c| {
        let file = c.state_dir.join("releases");
        std::fs::write(&file, b"x").expect("write");
        c.releases_dir = file;
    });
    assert!(
        sup.start_blocked()
            .is_some_and(|w| w.contains("releases directory"))
    );
}

/// Catches a startup hold that lets a `POST` through: `active` is a guess then, and the idle
/// slot looks idle. The hold's reason is in the answer; the migrator's is separate.
#[test]
fn a_startup_hold_blocks_with_its_reason() {
    let (sup, _dir) = over("blk-hold", |_, c| {
        c.startup_hold = Some("no frontend child is running".to_owned());
    });
    let why = sup.start_blocked().expect("blocked");
    assert!(why.contains("startup hold is in force"), "{why}");
    assert!(why.contains("no frontend child is running"), "{why}");
}

#[test]
fn a_migrator_that_can_never_pass_blocks_with_its_reason() {
    let (sup, _dir) = over("blk-migrator", |_, c| {
        c.migrator_blocked = Some("schema unknown".to_owned());
    });
    let why = sup.start_blocked().expect("blocked");
    assert!(why.contains("step migrate"), "{why}");
    assert!(why.contains("schema unknown"), "{why}");
}

#[test]
fn a_candidate_slot_that_still_runs_a_process_blocks() {
    let (sup, _dir) = over("blk-busy", |rig, _| rig.occupy(Slot::B));
    let why = sup.start_blocked().expect("blocked");
    assert!(why.contains("slot B is not idle"), "{why}");
}

#[test]
fn a_candidate_slot_whose_recorded_process_is_gone_is_idle() {
    let (sup, _dir) = over("blk-gone", |rig, _| {
        rig.occupy(Slot::B);
        rig.kill(Slot::B);
    });
    assert_eq!(sup.start_blocked(), None);
}

#[test]
fn the_slot_asked_about_is_the_one_that_does_not_serve() {
    // B serves (the record of a finished update says so) and A, where the old child was, is
    // gone: A is the idle slot.
    let (sup, _dir) = over("blk-active-b", |rig, c| {
        c.active = Slot::B;
        rig.occupy(Slot::B);
        rig.kill(Slot::A);
    });
    assert_eq!(sup.start_blocked(), None);
    // With the old child still running in A, the same host is blocked on A and not on B.
    let (sup, _dir) = over("blk-active-b-busy", |rig, c| {
        c.active = Slot::B;
        rig.occupy(Slot::B);
    });
    let why = sup.start_blocked().expect("blocked");
    assert!(why.contains("slot A is not idle"), "{why}");
}

#[test]
fn an_unreadable_update_record_blocks() {
    let dir = kbf_host::testkit::scratch("blk-unreadable");
    std::fs::write(dir.join("update.json"), b"not json").expect("write");
    let rig = Rig::new(Some(dir.clone()));
    let requests = Arc::new(RequestQueue::new());
    let parts = rig.parts(&requests, |_| {});
    let sup = SwapSupervisor::with_spawner(parts, Rig::config(dir), parked());
    let why = sup.start_blocked().expect("blocked");
    assert!(why.contains("update.json is unreadable"), "{why}");
}
