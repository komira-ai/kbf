//! The size of a heartbeat that uses every field at its bound.

use serde_json::{Value, json};

use crate::wire::{MAX_HEARTBEAT_BYTES, MAX_SERVICES, read_heartbeat};

/// The old fields of one beat (a service entry, or the top level) at their bounds.
fn old_fields() -> Value {
    json!({"version": "1".repeat(128), "health": "healthy",
        "last_update": {"target": "1".repeat(128), "outcome": "failed"},
        "updating": "1".repeat(128)})
}

/// The new facts of one beat at their bounds, with every number at the largest its field
/// holds (`step_secs` and `retry_after_secs` are `u64`, twenty digits).
fn new_facts(token: &str) -> Value {
    let step_secs: serde_json::Map<String, Value> = (0..8)
        .map(|i| (format!("{:_<16}", format!("s{i}")), json!(u64::MAX)))
        .collect();
    json!({
        "step": {"name": "n".repeat(16), "attempt": u32::MAX},
        "commit": "uncommitted",
        "step_secs": step_secs,
        "promotion_pending": true,
        "deferred": {"target": "1".repeat(128), "reason": token, "retry_after_secs": u64::MAX},
    })
}

/// A heartbeat of kbf and `MAX_SERVICES` services, each with its old fields at their bounds,
/// and the new facts too if `with_new`.
fn beat(with_new: bool) -> Vec<u8> {
    let token = "r".repeat(64);
    let entry = |name: Option<String>| {
        let mut entry = old_fields();
        entry["last_update"]["phase"] = json!("gate_environment");
        entry["last_update"]["class"] = json!("environment");
        entry["last_update"]["reason"] = json!(token);
        if let Some(name) = name {
            entry["name"] = json!(name);
        }
        if with_new {
            for (key, fact) in new_facts(&token).as_object().expect("an object") {
                entry[key] = fact.clone();
            }
        }
        entry
    };
    let services: Vec<Value> = (0..MAX_SERVICES)
        .map(|i| entry(Some(format!("{:a<62}{i:02}", ""))))
        .collect();
    let mut top = entry(None);
    top["platform"] = json!("linux-x86_64");
    top["services"] = json!(services);
    if with_new {
        top["agent_features"] = json!(
            (0..8)
                .map(|i| format!("{:r<30}{i:02}", ""))
                .collect::<Vec<_>>()
        );
    }
    serde_json::to_vec(&top).expect("json")
}

/// Pins the real worst case: a beat of kbf and sixteen services, every field at its bound and
/// every new fact too, is over `MAX_HEARTBEAT_BYTES` (the coordinator reads no more than
/// that), and the old fields alone are under it. So what keeps a beat under the cap is the
/// agent's own size check, which drops optional facts first, and not the bounds.
#[test]
fn a_beat_with_every_fact_at_its_bounds_is_over_the_cap_and_the_old_fields_alone_are_not() {
    let old = beat(false);
    let full = beat(true);
    assert!(old.len() < MAX_HEARTBEAT_BYTES, "old: {}", old.len());
    assert!(full.len() > MAX_HEARTBEAT_BYTES, "full: {}", full.len());
    read_heartbeat(&old).expect("the old beat at its bounds reads");
    read_heartbeat(&full).expect("the full beat reads, if it is all there");
}
