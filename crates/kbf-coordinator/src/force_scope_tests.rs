//! An operator's force over services beyond `kbf`'s shape: a role of the service's own naming,
//! a selector and the labels that move hosts in and out of it, and a bake cut short.

use std::sync::atomic::Ordering;

use kbf_types::HostRole;
use kbf_updater::wire::{Answer, Health, Heartbeat, ServiceBeat, ServiceReply};
use serde_json::json;

use crate::decide::Hold;
use crate::fleet::DOWN_AFTER;
use crate::force_tests::{GATED, agent, force, operate, restart, rig, send};
use crate::identity::Identity;
use crate::testutil::{OPERATOR, Rig, id, version};

/// `GATED`'s `kbf`, and `tool` on daemons and on `mac-daemon` hosts, which wait for the daemons
/// and go one at a time.
fn with_tool() -> String {
    format!(
        "{GATED}\n[[service]]\nname = \"tool\"\n\n[service.roles.daemon]\nmin_up = 0\n\n\
         [service.roles.mac-daemon]\nmin_up = 0\nmax_updating = 1\nafter = [\"daemon\"]\n"
    )
}

/// `GATED`'s `kbf`, and `zoned` on the daemons an operator labelled `zone=a`.
fn with_zoned() -> String {
    format!(
        "{GATED}\n[[service]]\nname = \"zoned\"\n\n[service.roles.daemon]\nmin_up = 0\n\n\
         [service.selector]\nlabels = {{ zone = \"a\" }}\n"
    )
}

/// The beat of an agent of this release that runs `kbf` at 2.0 and `service` at `running`, and
/// announces `heartbeat-v2` besides.
fn listing(service: &str, running: &str) -> Heartbeat {
    let mut b = agent("2.0", Health::Healthy);
    b.agent_features.push("heartbeat-v2".to_owned());
    let list = json!([{"name": service, "version": running, "health": "healthy"}]);
    b.services = Some(serde_json::from_value::<Vec<ServiceBeat>>(list).expect("beats"));
    b
}

fn send_as(rig: &Rig, name: &str, role: &str, b: Heartbeat) -> Answer {
    let who = Identity {
        node_id: id(name),
        role: role.parse::<HostRole>().expect("a role"),
    };
    rig.coordinator.heartbeat(who, b)
}

fn ordered(service: &str, target: &str) -> ServiceReply {
    ServiceReply::Update {
        service: Some(service.to_owned()),
        version: version(target),
    }
}

fn hold_in(rig: &Rig, service: &str, name: &str) -> Option<Hold> {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    status.per_service[service]
        .hosts
        .iter()
        .find(|h| h.host.node_id == name)
        .and_then(|h| h.hold)
}

fn in_status(rig: &Rig, service: &str, name: &str) -> bool {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    status.per_service[service]
        .hosts
        .iter()
        .any(|h| h.host.node_id == name)
}

/// Catches: a force that ignores what a service's own definition says of a role (services may
/// name only the four host roles, so the role is `mac-daemon` with a cap and an order this
/// service sets): its hosts wait for the role they come after, then go at the role's own cap
/// of one, and a force that raises that role's limit lets the others go at once.
#[tokio::test]
async fn a_force_paces_and_orders_a_role_of_the_services_own_naming() {
    let rig = rig("force-scope-mac-daemon", &with_tool());
    let body = r#"{"action":"force","service":"tool","version":"1.1","reason":"x"}"#;
    operate(&rig, body).expect("forced");
    let answer = send_as(&rig, "d1", "daemon", listing("tool", "1.0"));
    assert_eq!(answer, ordered("tool", "1.1"));
    assert!(answer.forced);
    for name in ["b1", "b2", "b3"] {
        let answer = send_as(&rig, name, "mac-daemon", listing("tool", "1.0"));
        assert_eq!(answer, ServiceReply::Nothing, "{name}");
        assert_eq!(hold_in(&rig, "tool", name), Some(Hold::WaitingForRole));
    }
    send_as(&rig, "d1", "daemon", listing("tool", "1.1"));
    let round: Vec<bool> = ["b1", "b2", "b3"]
        .iter()
        .map(|n| send_as(&rig, n, "mac-daemon", listing("tool", "1.0")) == ordered("tool", "1.1"))
        .collect();
    assert_eq!(round, [true, false, false]);
    assert_eq!(hold_in(&rig, "tool", "b2"), Some(Hold::NoSlot));
    let raised = r#"{"action":"force","service":"tool","version":"1.1","reason":"x","max_updating":{"mac-daemon":3}}"#;
    operate(&rig, raised).expect("forced");
    let round: Vec<bool> = ["b2", "b3"]
        .iter()
        .map(|n| send_as(&rig, n, "mac-daemon", listing("tool", "1.0")) == ordered("tool", "1.1"))
        .collect();
    assert_eq!(round, [true, true]);
}

/// Catches: a force that reaches hosts its service's selector leaves out, or misses one an
/// operator's labels bring in while it stands: an unlabelled daemon is not in the service and
/// gets no forced order, a labelled one does, and labelling the first brings it the order on
/// its next beat.
#[tokio::test]
async fn a_force_reaches_the_hosts_a_selector_counts_as_labels_change() {
    let rig = rig("force-scope-selector", &with_zoned());
    let label = |node: &str| {
        let body =
            format!(r#"{{"action":"set_labels","node_id":"{node}","labels":{{"zone":"a"}}}}"#);
        operate(&rig, &body).expect("labelled");
    };
    label("d1");
    let body = r#"{"action":"force","service":"zoned","version":"1.1","reason":"x","max_updating":{"daemon":2}}"#;
    operate(&rig, body).expect("forced");
    let answer = send_as(&rig, "d1", "daemon", listing("zoned", "1.0"));
    assert_eq!(answer, ordered("zoned", "1.1"));
    assert!(answer.forced);
    assert_eq!(
        send_as(&rig, "d2", "daemon", listing("zoned", "1.0")),
        ServiceReply::Nothing
    );
    assert!(!in_status(&rig, "zoned", "d2"));
    label("d2");
    let answer = send_as(&rig, "d2", "daemon", listing("zoned", "1.0"));
    assert_eq!(answer, ordered("zoned", "1.1"));
    assert!(answer.forced);
}

fn last_good(rig: &Rig) -> Option<String> {
    rig.coordinator
        .ops()
        .marks("kbf")
        .last_good
        .map(|v| v.to_string())
}

/// A farm of three hosts that baked 1.2, then forced to 1.5, which each host runs healthy for
/// two beats of the three the bake needs.
async fn half_baked(name: &str) -> Rig {
    let rig = rig(name, GATED);
    rig.set(r#"{"version":"1.2","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for _ in 0..3 {
        for name in ["d1", "s1", "f1"] {
            send(&rig, name, agent("1.2", Health::Healthy));
        }
    }
    assert_eq!(last_good(&rig).as_deref(), Some("1.2"));
    force(
        &rig,
        "1.5",
        r#","max_updating":{"daemon":1,"storage":1,"frontend":1}"#,
    );
    for _ in 0..2 {
        for name in ["d1", "s1", "f1"] {
            send(&rig, name, agent("1.5", Health::Healthy));
        }
    }
    assert_eq!(last_good(&rig).as_deref(), Some("1.2"));
    rig
}

/// Catches: a bake cut short counted as a whole one: after two healthy beats of three, a
/// silence past the down limit, or a coordinator restart, starts the count again, so one more
/// beat each records nothing, and only a fresh three record the forced version.
#[tokio::test]
async fn a_forced_bake_cut_short_by_silence_or_a_restart_starts_again() {
    let mut silent = half_baked("force-scope-bake-silent").await;
    let now = silent.clock.load(Ordering::SeqCst);
    silent.at(now + DOWN_AFTER.as_secs());
    let mut restarted = half_baked("force-scope-bake-restart").await;
    restart(&mut restarted, GATED);
    for rig in [&mut silent, &mut restarted] {
        for name in ["d1", "s1", "f1"] {
            send(rig, name, agent("1.5", Health::Healthy));
        }
        assert_eq!(last_good(rig).as_deref(), Some("1.2"));
        for _ in 0..2 {
            for name in ["d1", "s1", "f1"] {
                send(rig, name, agent("1.5", Health::Healthy));
            }
        }
        assert_eq!(last_good(rig).as_deref(), Some("1.5"));
    }
}

/// A failure of `target` that began at `started`.
fn failure_at(target: &str, started: u64) -> kbf_updater::wire::UpdateResult {
    kbf_updater::wire::UpdateResult {
        started_unix: Some(started),
        ..kbf_updater::wire::UpdateResult::new(version(target), kbf_updater::wire::Outcome::Failed)
    }
}

/// Catches: a retry that ends between two heartbeats, failing or halting just as the attempt
/// before it did, read as the acknowledged old report: the agent says when each attempt began,
/// so the new report differs, the failure holds the host (rule 4) and the halt halts the
/// service again, with no beat that showed the host updating.
#[tokio::test]
async fn a_retry_that_ends_between_beats_is_told_apart_by_when_it_began() {
    let rig = rig("force-scope-attempts", GATED);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let mut failed = agent("2.0", Health::Healthy);
    failed.last_update = Some(failure_at("1.5", 100));
    send(&rig, "d1", failed.clone());
    let mut halting = agent("2.0", Health::Healthy);
    halting.last_update = Some(kbf_updater::wire::UpdateResult {
        started_unix: Some(100),
        ..crate::force_tests::halted("1.5")
    });
    send(&rig, "s1", halting.clone());
    force(&rig, "1.5", r#","max_updating":{"daemon":1,"storage":1}"#);
    assert_eq!(
        send(&rig, "d1", failed.clone()),
        crate::force_tests::update("1.5")
    );
    assert_eq!(
        send(&rig, "s1", halting.clone()),
        crate::force_tests::update("1.5")
    );
    failed.last_update = Some(failure_at("1.5", 200));
    assert_eq!(send(&rig, "d1", failed), ServiceReply::Nothing);
    assert_eq!(hold_in(&rig, "kbf", "d1"), Some(Hold::FailedHere));
    halting.last_update = Some(kbf_updater::wire::UpdateResult {
        started_unix: Some(200),
        ..crate::force_tests::halted("1.5")
    });
    send(&rig, "s1", halting);
    let halt = rig
        .coordinator
        .ops()
        .marks("kbf")
        .halt
        .expect("halted again");
    assert_eq!(halt.node_id, "s1");
}
