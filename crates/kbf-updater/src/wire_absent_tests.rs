//! `absent`, the health of a service declared and never registered.

use serde::Deserialize;

use crate::wire::{Health, Heartbeat};

const ABSENT: &str = r#"{"platform":"linux-x86_64","version":"1.0","health":"absent"}"#;

/// The health of the heartbeats before `absent`, frozen with its strict read.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FrozenHealth {
    Healthy,
    Unhealthy,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct FrozenHeartbeat {
    health: FrozenHealth,
}

/// Catches: `absent` not being spelled as the contract spells it: it reads, and writes back
/// as `absent`.
#[test]
fn a_heartbeat_with_health_absent_is_read_and_written_back() {
    let read: Heartbeat = serde_json::from_str(ABSENT).expect("absent is a health");
    assert_ne!(read.health, Health::Healthy);
    let written: serde_json::Value = serde_json::to_value(&read).expect("json");
    assert_eq!(written["health"], "absent");
}

/// Catches: the reason an agent may send `absent` only after `heartbeat-v2` is advertised: a
/// coordinator from before it refuses the whole heartbeat.
#[test]
fn health_absent_is_refused_by_the_frozen_v0_reader() {
    assert!(serde_json::from_str::<FrozenHeartbeat>(ABSENT).is_err());
}
