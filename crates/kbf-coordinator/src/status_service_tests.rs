//! The status per service: every defined service lists the hosts that heartbeated it, with
//! the service's own `up`, slot and hold answers, and kbf's bytes stay where they were.

use kbf_oidc::testkit::{STATIC_ISSUER, config_text};
use kbf_types::HostRole;
use kbf_updater::wire::{Health, Heartbeat, ServiceBeat};
use serde_json::{Value, json};

use crate::identity::Identity;
use crate::services::Services;
use crate::testutil::{OPERATOR, Rig, beat, ci_token, id, updating, version};

const SERVICES: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n\
                        [[service]]\nname = \"third\"\n[service.roles.daemon]\nmin_up = 0\n";

fn who(name: &str, role: HostRole) -> Identity {
    Identity {
        node_id: id(name),
        role,
    }
}

/// `top` carrying the beats `(service, version)` of the services the host runs besides kbf.
fn announcing(mut top: Heartbeat, services: &[(&str, &str)]) -> Heartbeat {
    top.services = Some(
        services
            .iter()
            .map(|(name, running)| ServiceBeat {
                name: (*name).to_owned(),
                version: version(running),
                health: Health::Healthy,
                last_update: None,
                updating: None,
                step: None,
                commit: None,
                step_secs: Default::default(),
                promotion_pending: false,
                deferred: None,
            })
            .collect(),
    );
    top
}

/// A coordinator over kbf, `other` and `third`; kbf wants 2.0 and `other` wants 3.0, `third`
/// has no desired state. At second 2000 (past the quiet start) five kbf hosts and two
/// daemons that also run `other` heartbeat:
///
/// - `s1` storage at 2.0; `s2` storage at 1.0 and updating to 2.0; `s3` storage at 1.0,
///   behind with the one storage slot taken by `s2`;
/// - `d1` daemon at 2.0, running `other` at 3.0 and `third` at 1.0;
/// - `d2` daemon at 2.0, running `other` at 1.0: it is ordered to update `other`, so under
///   kbf it is busy elsewhere.
async fn farm(name: &str) -> Rig {
    let mut rig = Rig::new(name);
    let list = config_text(STATIC_ISSUER) + "services = [\"kbf\", \"other\", \"third\"]\n";
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(SERVICES).expect("services"));
    for body in [
        r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#,
        r#"{"service":"other","version":"3.0","min_up":{"storage":0,"frontend":0}}"#,
    ] {
        rig.coordinator
            .set_desired(Some(&ci_token()), body.as_bytes())
            .await
            .expect("set");
    }
    rig.at(2_000);
    let c = &rig.coordinator;
    c.heartbeat(who("s1", HostRole::STORAGE), beat("2.0"));
    c.heartbeat(who("s2", HostRole::STORAGE), updating(beat("1.0"), "2.0"));
    c.heartbeat(who("s3", HostRole::STORAGE), beat("1.0"));
    c.heartbeat(
        who("d1", HostRole::DAEMON),
        announcing(beat("2.0"), &[("other", "3.0"), ("third", "1.0")]),
    );
    c.heartbeat(
        who("d2", HostRole::DAEMON),
        announcing(beat("2.0"), &[("other", "1.0")]),
    );
    rig
}

fn status_of(rig: &Rig) -> Value {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    serde_json::to_value(status).expect("JSON")
}

/// One row per host of `service`: node id, `up`, `holds_slot` and `hold`.
fn rows(status: &Value, service: &str) -> Vec<(String, Value, Value, Value)> {
    status["per_service"][service]["hosts"]
        .as_array()
        .unwrap_or_else(|| panic!("{service} lists no hosts"))
        .iter()
        .map(|h| {
            (
                h["node_id"].as_str().expect("id").to_owned(),
                h["up"].clone(),
                h["holds_slot"].clone(),
                h["hold"].clone(),
            )
        })
        .collect()
}

fn row(node: &str, up: bool, slot: bool, hold: Value) -> (String, Value, Value, Value) {
    (node.to_owned(), json!(up), json!(slot), hold)
}

/// Behaviour: the status lists every defined service with the hosts that heartbeated it,
/// how many are up, how many hold a slot, and for each host the rule that holds an order
/// back (null when the host would be ordered). Catches a service listed without its hosts,
/// counts taken over another service's hosts, and a hold reason left out or taken for
/// another service.
#[tokio::test]
async fn status_lists_every_service_with_its_hosts_up_count_slots_and_hold() {
    let rig = farm("status-service-list").await;
    let status = status_of(&rig);
    let services: Vec<&str> = status["per_service"]
        .as_object()
        .expect("per_service")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(services, ["kbf", "other", "third"]);

    let kbf = &status["per_service"]["kbf"];
    assert_eq!(
        rows(&status, "kbf"),
        [
            row("d1", true, false, json!("up_to_date")),
            row("d2", true, false, json!("busy_elsewhere")),
            row("s1", true, false, json!("up_to_date")),
            row("s2", false, true, json!("updating")),
            row("s3", true, false, json!("no_slot")),
        ]
    );
    assert_eq!((&kbf["up"], &kbf["slots_held"]), (&json!(4), &json!(1)));

    let other = &status["per_service"]["other"];
    assert_eq!(
        rows(&status, "other"),
        [
            row("d1", true, false, json!("up_to_date")),
            row("d2", false, true, Value::Null),
        ]
    );
    assert_eq!((&other["up"], &other["slots_held"]), (&json!(1), &json!(1)));
    assert_eq!(other["convergence"]["state"], "in_progress");
    assert_eq!(other["convergence"]["desired"], "3.0");

    let third = &status["per_service"]["third"];
    assert_eq!(
        rows(&status, "third"),
        [row("d1", true, false, json!("no_desired"))]
    );
    assert_eq!((&third["up"], &third["slots_held"]), (&json!(1), &json!(0)));
    assert_eq!(third["desired"], Value::Null);
    assert_eq!(third["convergence"]["desired"], Value::Null);
}

/// Behaviour: a host holding a slot of one service is shown holding it for that service
/// only. `d2` was ordered to update `other`: under `other` it holds a slot and is not up;
/// under kbf, and in the top-level kbf host list, it holds nothing and is up. Catches the
/// kbf service being used where the service named should be.
#[tokio::test]
async fn an_order_for_another_service_is_shown_under_that_service_only() {
    let rig = farm("status-service-slot").await;
    let status = status_of(&rig);
    let d2 = |rows: Vec<(String, Value, Value, Value)>| {
        rows.into_iter().find(|r| r.0 == "d2").expect("d2")
    };
    let under_other = d2(rows(&status, "other"));
    assert_eq!((under_other.1, under_other.2), (json!(false), json!(true)));
    let under_kbf = d2(rows(&status, "kbf"));
    assert_eq!((under_kbf.1, under_kbf.2), (json!(true), json!(false)));
    let top = status["hosts"]
        .as_array()
        .expect("hosts")
        .iter()
        .find(|h| h["node_id"] == "d2")
        .expect("d2");
    assert_eq!(
        (&top["up"], &top["holds_slot"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(
        top.get("hold"),
        None,
        "the top-level host list is the old shape"
    );
}

/// Behaviour: the convergence in each service's status is the answer `GET /v1/convergence`
/// gives for the service, so the two cannot be read differently. Catches a status that
/// counts with another rule than the read does.
#[tokio::test]
async fn the_convergence_in_the_status_is_the_convergence_read() {
    let rig = farm("status-service-converge").await;
    let status = status_of(&rig);
    for service in ["kbf", "other", "third"] {
        let read = rig
            .coordinator
            .convergence(Some(&ci_token()), service)
            .await
            .expect("convergence");
        assert_eq!(
            status["per_service"][service]["convergence"],
            serde_json::to_value(read).expect("JSON"),
            "{service}"
        );
    }
}

const OLD_HEAD: &str = concat!(
    r#"{"desired":{"version":"2.0","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":1,"frontend":0}},"#,
    r#""services":{"kbf":{"version":"2.0","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":1,"frontend":0}}},"#,
    r#""quiet":false,"services_dropped":0,"hosts":[{"node_id":"st1","role":"storage","live":true,"up":true,"holds_slot":false,"silent_secs":0,"#,
    r#""platform":"linux-x86_64","version":"2.0","health":"healthy","last_update":null,"updating":null}],"#,
    r#""per_service":{"kbf":{"desired":{"version":"2.0","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":1,"frontend":0}},"#,
    r#""roles":[{"role":"storage","at_desired":1,"updating":0,"failed":0,"down":0}]"#
);

/// Pins: with only kbf defined, the status keeps every byte it had, up to and including
/// kbf's `roles`, and the new keys follow in the same order as they come for any service:
/// `hosts`, `up`, `slots_held`, `convergence`; `commit` stays last. Catches a renamed,
/// removed or moved old key, and a new key inserted among the old ones.
#[tokio::test]
async fn the_kbf_status_keeps_its_bytes_and_only_gains_keys() {
    let rig = Rig::new("status-service-bytes");
    rig.set(r#"{"version":"2.0","min_up":{"storage":1,"frontend":0}}"#)
        .await;
    rig.at(2_000);
    rig.coordinator
        .heartbeat(who("st1", HostRole::STORAGE), beat("2.0"));
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    let text = serde_json::to_string(&status).expect("JSON");
    let commit = crate::BUILD_COMMIT;
    let host = concat!(
        r#"{"node_id":"st1","role":"storage","live":true,"up":true,"holds_slot":false,"silent_secs":0,"#,
        r#""platform":"linux-x86_64","version":"2.0","health":"healthy","last_update":null,"updating":null,"hold":"up_to_date"}"#
    );
    let convergence = format!(
        r#"{{"desired":"2.0","quiet":false,"state":"converged","roles":[{{"role":"storage","at_desired":1,"updating":0,"failed":0,"down":0}}],"commit":"{commit}"}}"#
    );
    let want = format!(
        r#"{OLD_HEAD},"hosts":[{host}],"up":1,"slots_held":0,"convergence":{convergence}}}}},"commit":"{commit}"}}"#
    );
    assert_eq!(text, want);
}

/// Pins: the top-level keys of the status, which old clients read, are exactly those they
/// were, and `per_service` entries only add to `desired` and `roles`. Catches a key renamed
/// or dropped, whatever else is added.
#[tokio::test]
async fn the_status_keys_are_the_old_keys_plus_additions() {
    let rig = farm("status-service-keys").await;
    let status = status_of(&rig);
    let mut top: Vec<&str> = status
        .as_object()
        .expect("status")
        .keys()
        .map(String::as_str)
        .collect();
    top.sort_unstable();
    assert_eq!(
        top,
        [
            "commit",
            "desired",
            "hosts",
            "per_service",
            "quiet",
            "services",
            "services_dropped"
        ]
    );
    let mut service: Vec<&str> = status["per_service"]["kbf"]
        .as_object()
        .expect("kbf")
        .keys()
        .map(String::as_str)
        .collect();
    service.sort_unstable();
    assert_eq!(
        service,
        [
            "convergence",
            "desired",
            "hosts",
            "roles",
            "slots_held",
            "up"
        ]
    );
}
