//! A bad new fact inside one service's beat costs that fact only, never the host's services.

use serde_json::{Value, json};

use crate::wire::read_heartbeat;

fn service(name: &str) -> Value {
    json!({"name": name, "version": "1.0", "health": "healthy",
        "step": {"name": "verify", "attempt": 2}, "commit": "committed",
        "step_secs": {"cutover": 41}, "promotion_pending": true,
        "deferred": {"target": "1.2", "reason": "drain_bound", "retry_after_secs": 300}})
}

fn beat_with(services: Vec<Value>) -> Vec<u8> {
    let wire = json!({"platform": "linux-x86_64", "version": "1.0", "health": "healthy",
        "services": services});
    serde_json::to_vec(&wire).expect("json")
}

/// Catches: a bad new fact in one `ServiceBeat` dropping the host's whole services list, which
/// the coordinator would read as every service gone. Each bad fact is dropped alone and
/// reported with the service and the fact; the service stays, with its other facts, and so do
/// the other services.
#[test]
fn a_bad_new_fact_in_one_service_beat_loses_only_that_fact() {
    let nine: serde_json::Map<String, Value> =
        (0..9).map(|i| (format!("s{i}"), json!(1))).collect();
    let bad = [
        ("step", json!({"name": "a".repeat(17), "attempt": 1})),
        ("step", json!({"name": "verify"})),
        ("commit", json!("maybe")),
        ("step_secs", json!(nine)),
        ("step_secs", json!({"A": 1})),
        ("promotion_pending", json!("yes")),
        (
            "deferred",
            json!({"target": "1.2", "reason": "Busy", "retry_after_secs": 1}),
        ),
        (
            "deferred",
            json!({"target": "1.2", "reason": "x".repeat(65), "retry_after_secs": 1}),
        ),
    ];
    let good = read_heartbeat(&beat_with(vec![service("db"), service("web")]))
        .expect("read")
        .beat;
    for (key, value) in bad {
        let mut broken = service("db");
        broken[key] = value.clone();
        let got = read_heartbeat(&beat_with(vec![broken, service("web")])).expect("read");
        assert_eq!(got.services_dropped, None, "{key}: {value}");
        let why = got.extras_dropped.as_deref().unwrap_or_default();
        assert!(why.contains("db") && why.contains(key), "{key}: {why:?}");
        let services = got.beat.services.expect("the services are kept");
        let mut expected = good.services.clone().expect("services");
        match key {
            "step" => expected[0].step = None,
            "commit" => expected[0].commit = None,
            "step_secs" => expected[0].step_secs.clear(),
            "promotion_pending" => expected[0].promotion_pending = false,
            _ => expected[0].deferred = None,
        }
        assert_eq!(services, expected, "{key}");
    }
}

/// Catches: the structural rules loosened along with the facts: a name that is no service name,
/// a name listed twice, kbf as a service and too many entries still drop the list, and say so
/// as `services_dropped`, even when a fact is also bad.
#[test]
fn an_old_structural_fault_still_drops_the_whole_list() {
    let mut bad_fact = service("db");
    bad_fact["commit"] = json!("maybe");
    let many: Vec<Value> = (0..17).map(|i| service(&format!("s{i}"))).collect();
    let cases = [
        vec![service("Db")],
        vec![service("db"), service("db")],
        vec![service("kbf")],
        many,
        vec![bad_fact, service("kbf")],
    ];
    for services in cases {
        let got = read_heartbeat(&beat_with(services)).expect("read");
        assert!(got.services_dropped.is_some());
        assert_eq!(got.beat.services, None);
    }
}
