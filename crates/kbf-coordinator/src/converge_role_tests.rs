//! Convergence and status over roles named by definition: which roles count, and the order
//! they are listed in.

use std::time::Duration;

use kbf_types::HostRole;

use crate::converge::{State, converge, converge_service};
use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::services::{RoleDef, ServiceDef};
use crate::testutil::{OPERATOR, Rig, beat, desired, fleet, id, secs};

const NOW: Duration = Duration::from_secs(1_000);

/// The hosts of `fleet`, in the order of its first service: `converge`'s role names.
fn role_names(roles: &[crate::converge::RoleCounts]) -> Vec<&str> {
    roles
        .iter()
        .map(|r| AsRef::<str>::as_ref(&r.role))
        .collect()
}

/// Catches: roles listed alphabetically (`daemon`, `frontend`, `mac-daemon`, `storage`)
/// where the status and CI have always read frontend, storage, daemon, mac-daemon: hosts
/// heard in the reverse order come out in that order.
#[test]
fn kbf_roles_in_convergence_keep_the_legacy_order() {
    let f = fleet(
        secs(990),
        &[
            ("m1", HostRole::MAC_DAEMON, beat("2.0")),
            ("d1", HostRole::DAEMON, beat("2.0")),
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("f1", HostRole::FRONTEND, beat("2.0")),
        ],
    );
    let c = converge(Some(&desired("2.0", 1, 1)), &f, NOW);
    assert_eq!(
        role_names(&c.roles),
        ["frontend", "storage", "daemon", "mac-daemon"]
    );
}

/// Catches: roles the four legacy names do not cover listed before them or in map insertion
/// order: a definition with `zeta`, `alpha`, `daemon` and `frontend` lists frontend, then
/// daemon (the legacy order), then alpha, zeta (by name).
#[test]
fn roles_of_a_definition_come_legacy_first_then_by_name() {
    let def = ServiceDef::new(
        "pool",
        &[
            ("zeta", RoleDef::plain()),
            ("alpha", RoleDef::plain()),
            ("daemon", RoleDef::plain()),
            ("frontend", RoleDef::plain()),
        ],
    );
    let mut f = Fleet::new(Duration::ZERO);
    for role in ["zeta", "alpha", "daemon", "frontend"] {
        f.record_named_in(
            "pool",
            id(&format!("{role}-1")),
            role,
            beat("2.0"),
            secs(990),
        );
    }
    let c = converge_service(&def, Some(&desired("2.0", 0, 0)), &f, NOW);
    assert_eq!(
        role_names(&c.roles),
        ["frontend", "daemon", "alpha", "zeta"]
    );
    assert_eq!(c.state, State::Converged);
}

/// Catches: a host of a role the definition does not name entering the counts (it would
/// keep the service from converging, being behind): a stranger at the old version beside
/// a builder at the new one leaves the service converged, with one role listed.
#[test]
fn convergence_counts_only_the_roles_of_the_definition() {
    let def = ServiceDef::new("pool", &[("builder", RoleDef::plain())]);
    let mut f = Fleet::new(Duration::ZERO);
    f.record_named_in("pool", id("b1"), "builder", beat("2.0"), secs(990));
    f.record_named_in("pool", id("x1"), "stranger", beat("1.0"), secs(990));
    let d = Desired::parse(br#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .expect("parsed");
    let c = converge_service(&def, Some(&d), &f, NOW);
    assert_eq!(role_names(&c.roles), ["builder"]);
    assert_eq!(c.state, State::Converged);
}

/// Catches: the status listing a service's roles in another order than convergence does:
/// hosts of all four roles heard in the reverse order are listed frontend, storage,
/// daemon, mac-daemon under `kbf`.
#[tokio::test]
async fn status_lists_roles_in_the_legacy_order() {
    let rig = Rig::new("converge-role-status");
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    rig.at(2_000);
    for (name, role) in [
        ("m1", HostRole::MAC_DAEMON),
        ("d1", HostRole::DAEMON),
        ("s1", HostRole::STORAGE),
        ("f1", HostRole::FRONTEND),
    ] {
        let who = Identity {
            node_id: id(name),
            role,
        };
        rig.coordinator.heartbeat(who, beat("2.0"));
    }
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(
        role_names(&status.per_service["kbf"].roles),
        ["frontend", "storage", "daemon", "mac-daemon"]
    );
}
