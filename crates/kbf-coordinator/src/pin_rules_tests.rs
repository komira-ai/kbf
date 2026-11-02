//! A pinned host through the edges of the coordinator's rules: a role of pinned hosts alone, a
//! pinned host gone silent past every age, a canary pinned at each stage, and the automatic
//! rollback with pinned and unpinned failures, another service's failures, and a pinned host at
//! the desired version.

use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{Commit, Heartbeat, Outcome, PinReport};

use crate::bake_canary_tests::{beats, go, held, services, started, why};
use crate::converge::{State, converge_kbf};
use crate::decide::Hold;
use crate::desired::KBF;
use crate::fleet::{DOWN_AFTER, Fleet, ORDER_AGE};
use crate::ops::OpsState;
use crate::reconcile::{Change, plan};
use crate::rollback_tests::{AUTO, def, failure, farm, good, plan_for, release, rolled_back};
use crate::services::{ServiceDef, Services};
use crate::testutil::{beat, desired, id, last, secs, updating, version};

fn pinned(mut b: Heartbeat) -> Heartbeat {
    b.pinned = Some(PinReport {
        commit: Commit::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    });
    b
}

/// A role whose live hosts are all pinned behind the desired version is still listed, counted
/// pinned and neither failed nor at the desired version, and the farm is `in_progress`.
///
/// Catches: a role dropped from the counts because `pinned` is its only count.
#[test]
fn a_role_of_pinned_hosts_alone_is_listed_and_in_progress() {
    let mut f = Fleet::new(Duration::ZERO);
    for host in ["s1", "s2"] {
        let b = pinned(last(beat("1.0"), "2.0", Outcome::Failed));
        f.record(id(host), HostRole::STORAGE, b, secs(100));
    }
    let c = converge_kbf(
        &ServiceDef::kbf(),
        Some(&desired("2.0", 0, 0)),
        &f,
        secs(100),
    );
    assert_eq!(c.state, State::InProgress);
    assert_eq!(c.roles.len(), 1);
    let storage = &c.roles[0];
    assert_eq!(
        (
            storage.role.as_str(),
            storage.at_desired,
            storage.updating,
            storage.failed,
            storage.down,
            storage.pinned
        ),
        ("storage", 0, 0, 0, 0, 2)
    );
}

/// A pinned host keeps its slot past the down limit and past the age of the order it had been
/// given: the next host of its role stays held. When it heartbeats again unpinned and healthy,
/// the slot is free.
///
/// Catches: a pinned host's slot freed by silence or by an order's age.
#[test]
fn a_silent_pinned_host_keeps_its_slot_until_it_beats_unpinned() {
    let s = services("", 1, 0);
    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    f.order(KBF, id("a"), version("2.0"), secs(100));
    beats(&mut f, "a", &pinned(beat("1.0")), 115);
    let late = 115 + (DOWN_AFTER + ORDER_AGE).as_secs() + 1;
    for host in ["b", "c"] {
        beats(&mut f, host, &beat("1.0"), late);
    }
    assert!(!f.get(&id("a")).unwrap().is_live(secs(late)));
    assert!(!f.is_ordered(KBF, &id("a"), secs(late)));
    assert_eq!(why(&s, &f, "b", late), held(Hold::NoSlot));
    beats(&mut f, "a", &beat("1.0"), late + 15);
    assert_eq!(why(&s, &f, "b", late + 15), go());
}

/// A canary pinned at each stage. Ordered and then pinned: its order ends, it has not started,
/// and the next host takes its place. Pinned while updating, or after failing the desired
/// version: it has started, so it fills the canary and the others wait; unpinned after the
/// failure, it is held as failed and the others still wait. Pinned at the desired version and
/// up, it counts as baked, and the gate opens.
///
/// Catches: a gate that a pinned canary keeps shut for good, or that opens on a failed one.
#[test]
fn a_canary_pinned_at_each_stage() {
    let s = services("canary = { count = 1 }", 3, 0);

    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    assert_eq!(why(&s, &f, "a", 100), go());
    f.order(KBF, id("a"), version("2.0"), secs(100));
    assert_eq!(why(&s, &f, "b", 100), held(Hold::CanaryWait));
    beats(&mut f, "a", &pinned(beat("1.0")), 115);
    assert_eq!(why(&s, &f, "a", 115), held(Hold::Pinned));
    assert_eq!(why(&s, &f, "b", 115), go(), "a no longer started");
    assert_eq!(why(&s, &f, "c", 115), held(Hold::CanaryWait));

    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    beats(&mut f, "a", &pinned(updating(beat("1.0"), "2.0")), 115);
    assert_eq!(why(&s, &f, "b", 115), held(Hold::CanaryWait));
    beats(&mut f, "a", &pinned(beat("2.0")), 130);
    assert_eq!(why(&s, &f, "b", 130), go(), "baked at the desired version");

    let mut f = Fleet::new(Duration::ZERO);
    started(&mut f, 100);
    beats(
        &mut f,
        "a",
        &pinned(last(beat("1.0"), "2.0", Outcome::Failed)),
        115,
    );
    assert_eq!(why(&s, &f, "a", 115), held(Hold::Pinned));
    assert_eq!(why(&s, &f, "b", 115), held(Hold::CanaryWait));
    beats(&mut f, "a", &last(beat("1.0"), "2.0", Outcome::Failed), 130);
    assert_eq!(why(&s, &f, "a", 130), held(Hold::FailedHere));
    assert_eq!(why(&s, &f, "b", 130), held(Hold::CanaryWait));
}

/// In a mixed fleet the unpinned failures alone are counted: three failed hosts of six with
/// one pinned are two votes, the threshold, and the service rolls back; two with one pinned are
/// one vote, and it does not.
///
/// Catches: a pin that hides the other hosts' failures, or one counted as a vote anyway.
#[test]
fn only_the_unpinned_failures_count_in_a_mixed_fleet() {
    let (phase, class) = release();
    let automatic = def(AUTO);
    let pin_first = |mut f: Fleet| {
        let b = f.get(&id("d0")).unwrap().beat.clone();
        f.record(id("d0"), HostRole::DAEMON, pinned(b), secs(2_000));
        f
    };
    assert_eq!(
        plan_for(&automatic, &good(), &pin_first(farm(6, 3, phase, class))),
        rolled_back()
    );
    assert_eq!(
        plan_for(&automatic, &good(), &pin_first(farm(6, 2, phase, class))),
        vec![]
    );
}

/// The pin is `kbf`'s: hosts pinned for `kbf` whose `other` service failed its release are
/// votes for rolling `other` back.
///
/// Catches: a `kbf` pin that hides another service's failures.
#[test]
fn another_services_failures_on_kbf_pinned_hosts_still_roll_it_back() {
    let other = Services::parse(&format!(
        "[[service]]\nname = \"other\"\n{AUTO}\n[service.roles.daemon]\nmax_updating = 6\nmin_up = 0\n"
    ))
    .expect("services")
    .get("other")
    .expect("other")
    .clone();
    let (phase, class) = release();
    let mut f = Fleet::new(Duration::ZERO);
    for i in 0..6 {
        let host = id(&format!("d{i}"));
        f.record(
            host.clone(),
            HostRole::DAEMON,
            pinned(beat("1.0")),
            secs(2_000),
        );
        let b = if i < 2 {
            failure(phase, class, "2.0")
        } else {
            beat("1.0")
        };
        f.record_in("other", host, HostRole::DAEMON, b, secs(2_000));
    }
    let changes = plan(
        &other,
        &desired("2.0", 0, 0),
        &good(),
        &f,
        &OpsState::default(),
        secs(2_000),
    );
    assert_eq!(changes, rolled_back());
}

/// A host pinned at the desired version holds its slot, so the version is not recorded as the
/// last good one while the pin stands; without the pin it is.
///
/// Catches: a last good version recorded while a host is still held by an operator.
#[test]
fn a_pinned_host_at_the_desired_version_defers_the_last_good_record() {
    let automatic = def(AUTO);
    let at_desired = |pin: bool| {
        let mut f = Fleet::new(Duration::ZERO);
        for i in 0..3 {
            let b = if pin && i == 0 {
                pinned(beat("2.0"))
            } else {
                beat("2.0")
            };
            f.record(id(&format!("d{i}")), HostRole::DAEMON, b, secs(2_000));
        }
        f
    };
    assert_eq!(
        plan_for(&automatic, &good(), &at_desired(false)),
        vec![Change::LastGood(version("2.0"))]
    );
    assert_eq!(plan_for(&automatic, &good(), &at_desired(true)), vec![]);
}

/// A pinned host's `kbf` halt report raises no halt: root holds the host on a release, and its
/// report stops no other host. The same report from the host unpinned halts `kbf`.
///
/// Catches: a pin that stops the whole service through the halt it was meant to avoid.
#[tokio::test]
async fn a_pinned_hosts_halt_report_raises_no_halt() {
    let halt_of = |rig: &crate::testutil::Rig| {
        rig.coordinator
            .status(Some(crate::testutil::OPERATOR))
            .expect("status")
            .ops
            .services
            .get(KBF)
            .and_then(|m| m.halt.as_ref())
            .map(|h| h.node_id.clone())
    };
    let report = || {
        let mut b = beat("1.0");
        b.last_update = Some(kbf_updater::wire::UpdateResult {
            target: version("2.0"),
            outcome: Outcome::Failed,
            phase: Some(kbf_updater::wire::Phase::Halt),
            class: None,
            reason: kbf_updater::wire::Token::parse("disk_tainted"),
        });
        b
    };
    let who = crate::identity::Identity {
        node_id: id("d1"),
        role: HostRole::DAEMON,
    };
    for (name, pin, halted) in [
        ("pin-halt-pinned", true, None),
        ("pin-halt-unpinned", false, Some("d1".to_owned())),
    ] {
        let rig = crate::testutil::Rig::new(name);
        rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
            .await;
        rig.at(2_000);
        let b = if pin { pinned(report()) } else { report() };
        rig.coordinator.heartbeat(who.clone(), b);
        assert_eq!(halt_of(&rig), halted, "{name}");
    }
}
