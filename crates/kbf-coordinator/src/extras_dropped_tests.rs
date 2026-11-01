//! Dropped optional facts of a heartbeat: counted by fact name, and logged once per host per
//! fact per hour.

use kbf_types::HostRole;
use serde_json::{Value, json};

use crate::identity::Identity;
use crate::testutil::{OPERATOR, Rig, id};

const HOUR: u64 = 3_600;

fn who(name: &str) -> Identity {
    Identity {
        node_id: id(name),
        role: HostRole::STORAGE,
    }
}

fn counts(rig: &Rig) -> Value {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    serde_json::to_value(status).expect("JSON")["extras_dropped"].clone()
}

/// Catches: dropped facts going unseen in the status, or counted by host or per beat of a
/// fact. Each beat adds one to each fact it names; the count is by fact name only.
#[test]
fn a_dropped_fact_is_counted_by_name_in_the_status() {
    let rig = Rig::new("extras-count");
    assert_eq!(counts(&rig), Value::Null);
    rig.coordinator
        .note_extras_dropped(&who("a1"), "x", &["step", "commit"]);
    rig.coordinator
        .note_extras_dropped(&who("a2"), "x", &["step"]);
    assert_eq!(counts(&rig), json!({"commit": 1, "step": 2}));
}

/// Catches: the warn on every beat of a host that keeps sending a bad fact. A host and fact
/// log once; the same host and another fact, and another host and the same fact, log at
/// once; the first host and fact log again an hour after the first, and not a second before.
/// The count goes up on every call.
#[test]
fn a_host_and_fact_log_once_an_hour_and_every_beat_is_counted() {
    let rig = Rig::new("extras-limit");
    let note = |name: &str, facts: &[&'static str]| {
        rig.coordinator.note_extras_dropped(&who(name), "x", facts)
    };
    rig.at(10_000);
    assert!(note("a1", &["step"]));
    assert!(!note("a1", &["step"]));
    assert!(note("a1", &["commit"]));
    assert!(note("a2", &["step"]));
    rig.at(10_000 + HOUR - 1);
    assert!(!note("a1", &["step"]));
    rig.at(10_000 + HOUR);
    assert!(note("a1", &["step"]));
    assert!(!note("a1", &["step"]));
    assert_eq!(counts(&rig), json!({"commit": 1, "step": 6}));
}

/// Catches: a beat with several bad facts, one of them already logged this hour, going
/// unlogged: it logs if any of its facts is due.
#[test]
fn a_beat_logs_if_any_of_its_facts_is_due() {
    let rig = Rig::new("extras-any");
    rig.at(10_000);
    assert!(
        rig.coordinator
            .note_extras_dropped(&who("a1"), "x", &["step"])
    );
    assert!(
        rig.coordinator
            .note_extras_dropped(&who("a1"), "x", &["step", "commit"])
    );
    assert!(
        !rig.coordinator
            .note_extras_dropped(&who("a1"), "x", &["step", "commit"])
    );
}
