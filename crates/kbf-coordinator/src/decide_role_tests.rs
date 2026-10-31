//! The decision over services whose roles are named by their definition, not by the four
//! roles a host certificate can carry: what a named role gets by default, and that a role
//! no definition names gets nothing.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::Reply;

use crate::decide::decide_in;
use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::services::{RoleDef, ServiceDef, Services};
use crate::testutil::{beat, id, version};

const NOW: Duration = Duration::from_secs(1_000);

fn update() -> Reply {
    Reply::Update {
        version: version("2.0"),
    }
}

/// The service `pool`, which runs on one role, `builder`.
fn pool(role: RoleDef) -> Services {
    Services::from_defs(vec![ServiceDef::new("pool", &[("builder", role)])])
}

/// The desired state of `pool`: version 2.0 and `limits` (the text between the version and
/// the minimums, with its comma).
fn want(limits: &str) -> BTreeMap<String, Desired> {
    let text = format!(r#"{{"version":"2.0"{limits},"min_up":{{"storage":0,"frontend":0}}}}"#);
    BTreeMap::from([(
        "pool".to_owned(),
        Desired::parse(text.as_bytes()).expect("parsed"),
    )])
}

fn builders(hosts: &[&str]) -> Fleet {
    let mut fleet = Fleet::new(Duration::ZERO);
    for host in hosts {
        fleet.record_named_in("pool", id(host), "builder", beat("1.0"), NOW);
    }
    fleet
}

fn ask(services: &Services, all: &BTreeMap<String, Desired>, fleet: &Fleet, host: &str) -> Reply {
    decide_in(services, all, "pool", fleet, &id(host), NOW)
}

/// Catches: a role the definition names but the desired state does not mention getting no
/// limit (or the daemon's two): it gets one host updating at a time and no minimum up. Host
/// `b1` is ordered, and while its order stands `b2` is told nothing.
#[test]
fn a_role_the_definition_names_with_no_overrides_gets_the_default_and_is_ordered() {
    let services = pool(RoleDef::plain());
    let all = want("");
    let mut fleet = builders(&["b1", "b2"]);
    assert_eq!(ask(&services, &all, &fleet, "b1"), update());
    fleet.order("pool", id("b1"), version("2.0"), NOW);
    assert_eq!(ask(&services, &all, &fleet, "b2"), Reply::Nothing);
    assert_eq!(ask(&services, &all, &fleet, "b1"), update());
}

/// Catches: a host whose role no definition names being ordered for that service: a
/// `stranger` host of `pool` (which runs on `builder`) and a storage host of a service that
/// runs on daemons are both told nothing, with every other rule satisfied.
#[test]
fn a_host_whose_role_no_definition_names_is_never_ordered() {
    let services = pool(RoleDef::plain());
    let mut fleet = builders(&["b1"]);
    fleet.record_named_in("pool", id("x1"), "stranger", beat("1.0"), NOW);
    assert_eq!(ask(&services, &want(""), &fleet, "x1"), Reply::Nothing);
    assert_eq!(ask(&services, &want(""), &fleet, "b1"), update());

    let other = Services::parse("[[service]]\nname = \"other\"\n[service.roles.daemon]\n")
        .expect("services");
    let mut fleet = Fleet::new(Duration::ZERO);
    fleet.record_in("other", id("s1"), HostRole::STORAGE, beat("1.0"), NOW);
    fleet.record_in("other", id("d1"), HostRole::DAEMON, beat("1.0"), NOW);
    let text = r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#;
    let all = BTreeMap::from([(
        "other".to_owned(),
        Desired::parse(text.as_bytes()).expect("parsed"),
    )]);
    let ask = |host: &str| decide_in(&other, &all, "other", &fleet, &id(host), NOW);
    assert_eq!(ask("s1"), Reply::Nothing);
    assert_eq!(ask("d1"), update());
}

/// Catches: the slots and the minimum of a role that is not one of the four being read from
/// another role's numbers, or ignored. With two slots and one host that must stay up, of
/// three builders the first two are ordered and the third waits; with a minimum of three
/// nobody is ordered, there being only two others.
#[test]
fn a_non_legacy_role_is_ordered_by_its_own_min_up_and_slots() {
    let role = RoleDef {
        max_updating: Some(2),
        min_up: Some(1),
        ..RoleDef::plain()
    };
    let services = pool(role);
    let all = want("");
    let mut fleet = builders(&["b1", "b2", "b3"]);
    for host in ["b1", "b2"] {
        assert_eq!(ask(&services, &all, &fleet, host), update(), "{host}");
        fleet.order("pool", id(host), version("2.0"), NOW);
    }
    assert_eq!(ask(&services, &all, &fleet, "b3"), Reply::Nothing);

    let strict = pool(RoleDef {
        min_up: Some(3),
        ..RoleDef::plain()
    });
    let fleet = builders(&["b1", "b2", "b3"]);
    assert_eq!(ask(&strict, &all, &fleet, "b1"), Reply::Nothing);
}

/// Catches: the order of precedence for a role's limit changed: the definition's number
/// beats the desired state's, which beats the default. With a definition limit of one, a
/// desired limit of three changes nothing; without one, the desired limit of two lets a
/// second host go.
#[test]
fn the_definitions_limit_beats_the_desired_state_which_beats_the_default() {
    let two = want(r#","max_updating":{"builder":2}"#);
    let bound = pool(RoleDef {
        max_updating: Some(1),
        ..RoleDef::plain()
    });
    let mut fleet = builders(&["b1", "b2"]);
    fleet.order("pool", id("b1"), version("2.0"), NOW);
    assert_eq!(ask(&bound, &two, &fleet, "b2"), Reply::Nothing);
    let open = pool(RoleDef::plain());
    assert_eq!(ask(&open, &two, &fleet, "b2"), update());
    assert_eq!(ask(&open, &want(""), &fleet, "b2"), Reply::Nothing);
}
