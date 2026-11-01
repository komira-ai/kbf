//! Named slot pools: two services whose role of the same name is in the same pool share one
//! count of hosts holding a slot, each service comparing it with its own `max_updating`.
//! A pool of one member decides exactly as no pool.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::Outcome;

use crate::decide::{Decision, Hold, decide_why};
use crate::desired::{Desired, KBF};
use crate::fleet::Fleet;
use crate::services::{RoleDef, ServiceDef, Services};
use crate::testutil::{beat, desired, id, last, unhealthy, updating, version};

const NOW: Duration = Duration::from_secs(1_000);

/// A role with `max` slots, no minimum, in the pool `pool`.
fn role(max: u32, pool: Option<&str>) -> RoleDef {
    RoleDef {
        max_updating: Some(max),
        min_up: Some(0),
        pool: pool.map(str::to_owned),
        ..RoleDef::plain()
    }
}

/// Services `a` and `b`, each with the roles `(name, def)` given for it.
fn services(a: &[(&str, RoleDef)], b: &[(&str, RoleDef)]) -> Services {
    Services::from_defs(vec![ServiceDef::new("a", a), ServiceDef::new("b", b)])
}

/// Both services desired at 2.0.
fn want() -> BTreeMap<String, Desired> {
    ["a", "b"]
        .map(|name| (name.to_owned(), desired("2.0", 0, 0)))
        .into()
}

/// A fleet where every `(host, role)` heartbeated both services at 1.0, with `special`
/// replacing the beat of one service for one host.
fn fleet_of(
    hosts: &[(&str, HostRole)],
    special: &[(&str, &str, kbf_updater::wire::Heartbeat)],
) -> Fleet {
    let mut fleet = Fleet::new(Duration::ZERO);
    for (host, role) in hosts {
        for service in ["a", "b"] {
            let b = special
                .iter()
                .find(|(h, s, _)| h == host && *s == service)
                .map_or_else(|| beat("1.0"), |(_, _, b)| b.clone());
            fleet.record_in(service, id(host), role.clone(), b, NOW);
        }
    }
    fleet
}

fn daemons<'a>(names: &[&'a str]) -> Vec<(&'a str, HostRole)> {
    names.iter().map(|n| (*n, HostRole::DAEMON)).collect()
}

fn ask(services: &Services, fleet: &Fleet, service: &str, host: &str) -> Decision {
    decide_why(services, &want(), service, fleet, &id(host), NOW)
}

fn update() -> Decision {
    Decision::Update {
        version: version("2.0"),
    }
}

/// Catches: a pool that counts only its own service's hosts. With one slot each, d1 ordered
/// for `a`, the other daemon is held for `b` (and for `a`) for want of a slot; with no pool
/// it would be ordered for `b`.
#[test]
fn two_services_in_one_pool_share_the_slot_on_a_role() {
    let shared = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(1, Some("p")))],
    );
    let mut fleet = fleet_of(&daemons(&["d1", "d2"]), &[]);
    assert_eq!(ask(&shared, &fleet, "a", "d1"), update());
    fleet.order("a", id("d1"), version("2.0"), NOW);
    assert_eq!(
        ask(&shared, &fleet, "b", "d2"),
        Decision::Hold(Hold::NoSlot)
    );
    assert_eq!(
        ask(&shared, &fleet, "a", "d2"),
        Decision::Hold(Hold::NoSlot)
    );

    let alone = services(&[("daemon", role(1, None))], &[("daemon", role(1, None))]);
    assert_eq!(ask(&alone, &fleet, "b", "d2"), update());
}

/// Catches: a failed update of one service not holding the pool's slot for the other (design
/// 5.2's cross-service case). d1 failed an update of `a` and is unhealthy: d2 is not ordered
/// for `b`. Once d1 heartbeats healthy again the slot is free.
#[test]
fn a_failed_host_of_one_service_holds_the_pool_slot_for_the_other() {
    let shared = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(1, Some("p")))],
    );
    let hosts = daemons(&["d1", "d2"]);
    let broken = unhealthy(last(beat("1.0"), "2.0", Outcome::Failed));
    let fleet = fleet_of(&hosts, &[("d1", "a", broken)]);
    assert_eq!(
        ask(&shared, &fleet, "b", "d2"),
        Decision::Hold(Hold::NoSlot)
    );

    let healed = last(beat("1.0"), "2.0", Outcome::Failed);
    let fleet = fleet_of(&hosts, &[("d1", "a", healed)]);
    assert_eq!(ask(&shared, &fleet, "b", "d2"), update());
}

/// Catches: pools matched too loosely. Different pool names, or a pool on one service and
/// none on the other, share nothing: d2 is ordered for `b` while d1 updates `a`.
#[test]
fn different_pools_or_none_do_not_share() {
    let fleet = fleet_of(
        &daemons(&["d1", "d2"]),
        &[("d1", "a", updating(beat("1.0"), "2.0"))],
    );
    let two_pools = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(1, Some("q")))],
    );
    assert_eq!(ask(&two_pools, &fleet, "b", "d2"), update());
    let one_pool = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(1, None))],
    );
    assert_eq!(ask(&one_pool, &fleet, "b", "d2"), update());
    assert_eq!(
        ask(&one_pool, &fleet, "a", "d2"),
        Decision::Hold(Hold::NoSlot)
    );
}

/// Catches: a pool that spans roles. Daemon and storage roles of both services are in the
/// pool `p`: a daemon updating `a` holds the daemon slot, not the storage one, and a role
/// named in the pool by one service only does not share with the other's role of that name.
#[test]
fn a_pool_is_scoped_by_role() {
    let hosts = [
        ("d1", HostRole::DAEMON),
        ("d2", HostRole::DAEMON),
        ("s1", HostRole::STORAGE),
    ];
    let fleet = fleet_of(&hosts, &[("d1", "a", updating(beat("1.0"), "2.0"))]);
    let both = services(
        &[
            ("daemon", role(1, Some("p"))),
            ("storage", role(1, Some("p"))),
        ],
        &[
            ("daemon", role(1, Some("p"))),
            ("storage", role(1, Some("p"))),
        ],
    );
    assert_eq!(ask(&both, &fleet, "b", "d2"), Decision::Hold(Hold::NoSlot));
    assert_eq!(ask(&both, &fleet, "b", "s1"), update());

    let half = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(1, None)), ("storage", role(1, Some("p")))],
    );
    assert_eq!(ask(&half, &fleet, "b", "d2"), update());
}

/// Catches: slots summed per service instead of counted per host. d1 holds a slot for both
/// services (failed one, updating the other), so the pool has one host in use, and with two
/// slots d2 is ordered.
#[test]
fn a_host_holding_two_services_slots_counts_once() {
    let shared = services(
        &[("daemon", role(2, Some("p")))],
        &[("daemon", role(2, Some("p")))],
    );
    let broken = unhealthy(last(beat("1.0"), "2.0", Outcome::Failed));
    let fleet = fleet_of(
        &daemons(&["d1", "d2", "d3"]),
        &[
            ("d1", "a", broken),
            ("d1", "b", updating(beat("1.0"), "2.0")),
        ],
    );
    assert_eq!(ask(&shared, &fleet, "a", "d2"), update());
    let mut fleet = fleet;
    fleet.order("a", id("d2"), version("2.0"), NOW);
    assert_eq!(
        ask(&shared, &fleet, "b", "d3"),
        Decision::Hold(Hold::NoSlot)
    );
}

/// Catches: the pool using one limit for every member. `a` allows one host, `b` two; with one
/// host in the pool `a` is held and `b` is not.
#[test]
fn each_service_compares_the_pool_with_its_own_limit() {
    let shared = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(2, Some("p")))],
    );
    let fleet = fleet_of(
        &daemons(&["d1", "d2"]),
        &[("d1", "a", updating(beat("1.0"), "2.0"))],
    );
    assert_eq!(
        ask(&shared, &fleet, "a", "d2"),
        Decision::Hold(Hold::NoSlot)
    );
    assert_eq!(ask(&shared, &fleet, "b", "d2"), update());
}

/// Catches: a pool reaching into `min_up`. d1 updates `b` and is still up for `a`, whose
/// `min_up` of one is met by it, so d2 is ordered for `a`.
#[test]
fn a_pool_does_not_touch_min_up() {
    let mut keep = role(2, Some("p"));
    keep.min_up = Some(1);
    let shared = services(&[("daemon", keep)], &[("daemon", role(2, Some("p")))]);
    let fleet = fleet_of(
        &daemons(&["d1", "d2"]),
        &[("d1", "b", updating(beat("1.0"), "2.0"))],
    );
    assert_eq!(ask(&shared, &fleet, "a", "d2"), update());
}

/// Catches: the shipped kbf definition, with its `storage` pool, deciding differently from
/// the same definition without it. Over a fleet of every role, in three states (nothing
/// moving, a storage host updating, a storage host failed), every host's decision is the
/// same with and without the pool, and the storage slot is held in the second state.
#[test]
fn kbf_alone_with_its_pool_decides_as_the_golden() {
    let pooled = Services::default();
    let mut bare = ServiceDef::kbf();
    for def in bare.roles.values_mut() {
        def.pool = None;
    }
    let bare = Services::from_defs(vec![bare]);
    let hosts = [
        ("f1", HostRole::FRONTEND),
        ("f2", HostRole::FRONTEND),
        ("s1", HostRole::STORAGE),
        ("s2", HostRole::STORAGE),
        ("s3", HostRole::STORAGE),
        ("d1", HostRole::DAEMON),
        ("d2", HostRole::DAEMON),
        ("d3", HostRole::DAEMON),
        ("m1", HostRole::MAC_DAEMON),
    ];
    let states = [
        beat("1.0"),
        updating(beat("1.0"), "2.0"),
        unhealthy(last(beat("1.0"), "2.0", Outcome::Failed)),
    ];
    let all = BTreeMap::from([(KBF.to_owned(), desired("2.0", 1, 1))]);
    for state in states {
        let mut fleet = Fleet::new(Duration::ZERO);
        for (host, role) in &hosts {
            let b = if *host == "s1" {
                state.clone()
            } else {
                beat("1.0")
            };
            fleet.record(id(host), role.clone(), b, NOW);
        }
        let mut told = Vec::new();
        for (host, _) in &hosts {
            let with = decide_why(&pooled, &all, KBF, &fleet, &id(host), NOW);
            let without = decide_why(&bare, &all, KBF, &fleet, &id(host), NOW);
            assert_eq!(with, without, "{host}");
            told.push(with);
        }
        let s2 = hosts.iter().position(|(h, _)| *h == "s2").expect("s2");
        if state.updating.is_some() || state.last_update.is_some() {
            assert_eq!(told[s2], Decision::Hold(Hold::NoSlot));
        } else {
            assert_eq!(told[s2], update());
        }
    }
}

/// Catches: a pool name the services file accepts but nothing can match. A role may name a
/// pool in the rules of a service name; an empty or upper-case one is refused, naming the
/// service.
#[test]
fn a_pool_name_is_validated_and_kept() {
    let file = |pool: &str| {
        format!("[[service]]\nname = \"a\"\n[service.roles.daemon]\npool = \"{pool}\"\n")
    };
    let services = Services::parse(&file("p-1")).expect("parsed");
    assert_eq!(
        services.get("a").expect("a").roles["daemon"]
            .pool
            .as_deref(),
        Some("p-1")
    );
    for bad in ["", "P", "a b"] {
        let why = Services::parse(&file(bad))
            .expect_err("refused")
            .to_string();
        assert!(why.contains("a: pool"), "{bad:?}: {why}");
    }
    let none = "[[service]]\nname = \"a\"\n[service.roles.daemon]\n";
    let services = Services::parse(none).expect("parsed");
    assert_eq!(services.get("a").expect("a").roles["daemon"].pool, None);
}

/// Catches: a slot freed by one service not freeing the pool for the other. d1 updating `a`
/// holds the one shared slot and d2 is held for `b`; once d1 reports `a` at 2.0 and done, the
/// slot is free and d2 is ordered for `b`.
#[test]
fn a_slot_freed_by_one_service_is_free_for_the_other() {
    let shared = services(
        &[("daemon", role(1, Some("p")))],
        &[("daemon", role(1, Some("p")))],
    );
    let hosts = daemons(&["d1", "d2"]);
    let busy = fleet_of(&hosts, &[("d1", "a", updating(beat("1.0"), "2.0"))]);
    assert_eq!(ask(&shared, &busy, "b", "d2"), Decision::Hold(Hold::NoSlot));
    let done = fleet_of(&hosts, &[("d1", "a", beat("2.0"))]);
    assert_eq!(ask(&shared, &done, "b", "d2"), update());
}

/// Catches: a pool that counts the other services' updating hosts but not their baking ones.
/// `a` bakes for three beats and d1 has sent `a` one at 2.0: it holds the shared slot for
/// baking, so d2 is held for `b` (with no pool `b` would order it).
#[test]
fn a_host_baking_for_one_service_holds_the_pool_slot_for_the_other() {
    let mut a = ServiceDef::new("a", &[("daemon", role(1, Some("p")))]);
    a.bake_beats = 3;
    let b = ServiceDef::new("b", &[("daemon", role(1, Some("p")))]);
    let fleet = fleet_of(&daemons(&["d1", "d2"]), &[("d1", "a", beat("2.0"))]);
    let shared = Services::from_defs(vec![a.clone(), b.clone()]);
    assert_eq!(
        ask(&shared, &fleet, "b", "d2"),
        Decision::Hold(Hold::Baking)
    );
    a.roles.get_mut("daemon").expect("daemon").pool = None;
    let alone = Services::from_defs(vec![a, ServiceDef::new("b", &[("daemon", role(1, None))])]);
    assert_eq!(ask(&alone, &fleet, "b", "d2"), update());
}

/// Catches: the shipped definitions not sharing storage. With kbf's shipped definition and
/// the shipped tierstore entry, a storage host updating tierstore holds the storage slot kbf
/// counts, so no other storage host is ordered kbf; a daemon, outside the pool, still is.
#[test]
fn the_shipped_kbf_and_tierstore_share_storage() {
    let shipped = Services::parse(crate::services::TIERSTORE_SERVICES).expect("parsed");
    let want = BTreeMap::from([
        (KBF.to_owned(), desired("2.0", 1, 1)),
        ("tierstore".to_owned(), desired("2.0", 0, 0)),
    ]);
    let storage = ["s1", "s2", "s3", "s4", "s5", "s6"];
    let ask = |fleet: &Fleet, host: &str| decide_why(&shipped, &want, KBF, fleet, &id(host), NOW);
    let fleet_with = |s1: kbf_updater::wire::Heartbeat| {
        let mut fleet = Fleet::new(Duration::ZERO);
        for host in storage {
            let ts = if host == "s1" {
                s1.clone()
            } else {
                beat("1.0")
            };
            fleet.record_in("tierstore", id(host), HostRole::STORAGE, ts, NOW);
            fleet.record_in(KBF, id(host), HostRole::STORAGE, beat("1.0"), NOW);
        }
        fleet.record_in(KBF, id("d1"), HostRole::DAEMON, beat("1.0"), NOW);
        fleet
    };
    let busy = fleet_with(updating(beat("1.0"), "2.0"));
    assert_eq!(ask(&busy, "s2"), Decision::Hold(Hold::NoSlot));
    assert_eq!(ask(&busy, "d1"), update());
    let idle = fleet_with(beat("1.0"));
    assert_eq!(ask(&idle, "s2"), update());
}
