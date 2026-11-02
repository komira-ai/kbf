//! The automatic rollback counts the hosts the service counts: a host an operator excluded, or
//! one the selector leaves out, neither fails the release nor makes the threshold larger.

use std::collections::BTreeMap;

use kbf_updater::wire::Health;

use crate::fleet::{Fleet, Node};
use crate::membership::HEARTBEAT_V2;
use crate::ops::{Exclusion, HostOps, OpsState};
use crate::reconcile::{Change, plan};
use crate::rollback_tests::{AUTO, farm, good, release, rolled_back, secs};
use crate::services::{ServiceDef, Services};
use crate::testutil::{desired, id};

/// `kbf` as a service of daemons chosen by the label `zone = "a"`, with the definition lines
/// `extra` in its table.
fn zoned(extra: &str) -> ServiceDef {
    let text = format!(
        "[[service]]\nname = \"kbf\"\n{AUTO}\n{extra}\n[service.roles.daemon]\n\
         max_updating = 8\nmin_up = 0\n[service.selector]\nlabels = {{ zone = \"a\" }}\n"
    );
    Services::parse(&text)
        .expect("services")
        .get("kbf")
        .expect("kbf")
        .clone()
}

/// `n` daemons at 1.0, the first two of which failed their update to 2.0, each known as a node
/// that announced `kbf` and `heartbeat-v2`.
fn fleet(n: usize) -> Fleet {
    let (phase, class) = release();
    let mut fleet = farm(n, 2, phase, class);
    for i in 0..n {
        fleet.note_node(
            id(&format!("d{i}")),
            Node {
                role: "daemon".to_owned(),
                features: vec![HEARTBEAT_V2.to_owned()],
                seen: secs(2_000),
                announced: BTreeMap::from([("kbf".to_owned(), Health::Healthy)]),
            },
        );
    }
    fleet
}

/// The operator's state in which the daemons `zone_a` are in zone a.
fn zone_a(zone_a: &[&str]) -> OpsState {
    let mut ops = OpsState::default();
    for name in zone_a {
        ops.hosts.insert(
            (*name).to_owned(),
            HostOps {
                labels: BTreeMap::from([("zone".to_owned(), "a".to_owned())]),
            },
        );
    }
    ops
}

fn exclude(ops: &mut OpsState, name: &str) {
    ops.excluded.push(Exclusion {
        service: "kbf".to_owned(),
        node_id: name.to_owned(),
        reason: "disk replaced".to_owned(),
    });
}

fn plan_with(def: &ServiceDef, fleet: &Fleet, ops: &OpsState) -> Vec<Change> {
    plan(def, &desired("2.0", 0, 0), &good(), fleet, ops, secs(2_000))
}

/// Two of six hosts failed, but an operator excluded both: the six are four to the service, none
/// of them failed, and nothing rolls back.
#[test]
fn failures_of_excluded_hosts_do_not_roll_back() {
    let mut ops = zone_a(&["d0", "d1", "d2", "d3", "d4", "d5"]);
    exclude(&mut ops, "d0");
    exclude(&mut ops, "d1");
    assert_eq!(plan_with(&zoned(""), &fleet(6), &ops), vec![]);
}

/// Two of six hosts failed, and the selector does not choose either of them: they are not the
/// service's hosts, and nothing rolls back.
#[test]
fn failures_of_unselected_hosts_do_not_roll_back() {
    let ops = zone_a(&["d2", "d3", "d4", "d5"]);
    assert_eq!(plan_with(&zoned(""), &fleet(6), &ops), vec![]);
}

/// Two selected hosts failed, among two healthy selected ones, an unselected one and an
/// excluded one: the threshold (two) is reached by the service's own hosts, and it rolls back.
#[test]
fn failures_of_selected_hosts_still_roll_back() {
    let mut ops = zone_a(&["d0", "d1", "d2", "d3", "d4"]);
    exclude(&mut ops, "d4");
    assert_eq!(plan_with(&zoned(""), &fleet(6), &ops), rolled_back());
}

/// At 50% two failures of four selected hosts are enough, whatever else is on the farm: four
/// healthy hosts the selector leaves out must not raise the threshold to four.
#[test]
fn unselected_hosts_do_not_dilute_the_failures_of_selected_ones() {
    let ops = zone_a(&["d0", "d1", "d2", "d3"]);
    let def = zoned("rollback_after_failed = { percent = 50 }");
    assert_eq!(plan_with(&def, &fleet(8), &ops), rolled_back());
}

/// The same at 50% with excluded hosts: four healthy excluded hosts are not in the count either.
#[test]
fn excluded_hosts_do_not_dilute_the_failures_of_selected_ones() {
    let mut ops = zone_a(&["d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7"]);
    for name in ["d4", "d5", "d6", "d7"] {
        exclude(&mut ops, name);
    }
    let def = zoned("rollback_after_failed = { percent = 50 }");
    assert_eq!(plan_with(&def, &fleet(8), &ops), rolled_back());
}
