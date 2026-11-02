//! The names of the optional facts the coordinator's reader dropped, which it counts.

use serde_json::{Value, json};

use crate::wire::read_heartbeat;

fn service(name: &str) -> Value {
    json!({"name": name, "version": "1.0", "health": "healthy"})
}

fn read(extra: &[(&str, Value)], services: Vec<Value>) -> Vec<&'static str> {
    let mut wire = json!({"platform": "linux-x86_64", "version": "1.0", "health": "healthy"});
    for (key, value) in extra {
        wire[key] = value.clone();
    }
    if !services.is_empty() {
        wire["services"] = json!(services);
    }
    read_heartbeat(&serde_json::to_vec(&wire).expect("json"))
        .expect("read")
        .extras_dropped_facts
}

/// Catches: a fact that is dropped but not named, so that it cannot be counted by name. Each
/// of the six facts, bad at the top level, is the one name reported.
#[test]
fn each_bad_top_level_fact_is_reported_by_its_name() {
    let bad = [
        ("agent_features", json!(["Bad Name"])),
        ("step", json!({"name": "verify"})),
        ("commit", json!("maybe")),
        ("step_secs", json!({"A": 1})),
        ("promotion_pending", json!("yes")),
        (
            "deferred",
            json!({"target": "1.2", "reason": "Busy", "retry_after_secs": 1}),
        ),
    ];
    for (key, value) in bad {
        assert_eq!(read(&[(key, value)], Vec::new()), [key], "{key}");
    }
}

/// Catches: a name reported once per bad value, or in the order the beat listed them. Two bad
/// facts are reported sorted, and the same fact bad at the top level and in two service
/// entries is reported once.
#[test]
fn the_names_are_sorted_and_each_is_reported_once() {
    let mut first = service("db");
    first["step"] = json!({"name": "verify"});
    let mut second = service("web");
    second["step"] = json!({"name": "verify"});
    second["commit"] = json!("maybe");
    let got = read(
        &[
            ("step", json!({"name": "verify"})),
            ("commit", json!("maybe")),
        ],
        vec![first, second],
    );
    assert_eq!(got, ["commit", "step"]);
}

/// Catches: a name reported for a heartbeat that lost nothing.
#[test]
fn a_good_beat_names_no_fact() {
    let got = read(&[("commit", json!("committed"))], vec![service("db")]);
    assert_eq!(got, Vec::<&str>::new());
}
