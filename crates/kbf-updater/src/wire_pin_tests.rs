//! The heartbeat's `pinned`: its bytes, what a coordinator from before it does with it, what an
//! agent from before it sends, and what this coordinator's reader does with a bad one.

use serde::Deserialize;

use crate::version::Version;
use crate::wire::{Commit, Health, Heartbeat, PinReport, read_heartbeat};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn beat(pinned: Option<PinReport>) -> Heartbeat {
    let mut beat = serde_json::from_str::<Heartbeat>(
        r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":null,"updating":null}"#,
    )
    .expect("a heartbeat");
    beat.pinned = pinned;
    beat
}

/// Pins the bytes of the new field: `pinned` is the last key, an object with the commit, and an
/// empty object for a pin whose commit is not known.
///
/// Catches: the field renamed, written when there is no pin, or its commit spelled otherwise.
#[test]
fn the_pinned_field_is_written_as_the_golden_says() {
    let pinned = beat(Some(PinReport {
        commit: Commit::parse(COMMIT),
    }));
    assert_eq!(
        serde_json::to_string(&pinned).expect("json"),
        format!(
            r#"{{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":null,"updating":null,"pinned":{{"commit":"{COMMIT}"}}}}"#
        )
    );
    let unknown = beat(Some(PinReport { commit: None }));
    assert_eq!(
        serde_json::to_string(&unknown).expect("json"),
        r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":null,"updating":null,"pinned":{}}"#
    );
    assert_eq!(
        serde_json::to_string(&beat(None)).expect("json"),
        r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":null,"updating":null}"#
    );
    for one in [pinned, unknown] {
        let text = serde_json::to_string(&one).expect("json");
        assert_eq!(serde_json::from_str::<Heartbeat>(&text).expect("read"), one);
    }
}

/// The heartbeat struct of the release before the pin, frozen: no `deny_unknown_fields`, and
/// none of the fields it did not know.
#[derive(Deserialize)]
#[allow(dead_code)] // The fields are read, not used: the read is what is tested.
struct PreviousHeartbeat {
    platform: String,
    version: String,
    health: String,
    #[serde(default)]
    last_update: Option<serde_json::Value>,
    #[serde(default)]
    updating: Option<String>,
    #[serde(default)]
    agent_features: Vec<String>,
    #[serde(default)]
    deferred: Option<serde_json::Value>,
}

/// An old coordinator still accepts a pinned host's heartbeat: the previous release's reader
/// reads past `pinned` and keeps the rest. And vice versa: a heartbeat from an agent that knows
/// no pin is read by this release as not pinned, and written back byte for byte.
///
/// Catches: a pin that costs a host its heartbeat at a coordinator from before it; a field made
/// required.
#[test]
fn an_old_coordinator_accepts_a_pinned_heartbeat_and_an_old_agent_is_not_pinned() {
    let mut pinned = beat(Some(PinReport {
        commit: Commit::parse(COMMIT),
    }));
    pinned.agent_features = vec!["deferred-v1".to_owned()];
    let bytes = serde_json::to_vec(&pinned).expect("json");
    let old: PreviousHeartbeat = serde_json::from_slice(&bytes).expect("the old reader reads it");
    assert_eq!(
        (old.version.as_str(), old.health.as_str()),
        ("1.0", "healthy")
    );
    assert_eq!(old.agent_features, ["deferred-v1"]);

    let from_old_agent = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":null,"updating":null}"#;
    let read = read_heartbeat(from_old_agent.as_bytes()).expect("read");
    assert_eq!(read.beat.pinned, None);
    assert_eq!(read.extras_dropped, None);
    assert_eq!(
        serde_json::to_string(&read.beat).expect("json"),
        from_old_agent
    );
}

/// A `pinned` this reader cannot read (a commit that is not 40 hex digits, a value that is no
/// object) is dropped on its own and counted as `pinned`; the host keeps its heartbeat. A key a
/// later agent adds inside `pinned` is ignored.
///
/// Catches: a bad pin that makes the coordinator refuse the whole heartbeat; a pin refused for
/// a key it does not know.
#[test]
fn a_bad_pinned_is_dropped_and_an_unknown_key_in_it_is_ignored() {
    let base = r#"{"platform":"linux-x86_64","version":"1.0","health":"unhealthy","last_update":null,"updating":null"#;
    for bad in [r#""pinned":{"commit":"ABC"}"#, r#""pinned":7"#] {
        let read = read_heartbeat(format!("{base},{bad}}}").as_bytes()).expect("kept");
        assert_eq!(read.beat.pinned, None, "{bad}");
        assert_eq!(read.beat.health, Health::Unhealthy);
        assert_eq!(read.extras_dropped_facts, ["pinned"], "{bad}");
        assert!(
            read.extras_dropped
                .as_deref()
                .is_some_and(|why| why.starts_with("pinned: ")),
            "{bad}"
        );
    }
    let later = format!(r#"{base},"pinned":{{"commit":"{COMMIT}","by":"force","at_unix":9}}}}"#);
    let read = read_heartbeat(later.as_bytes()).expect("read");
    assert_eq!(read.extras_dropped, None);
    assert_eq!(
        read.beat.pinned.and_then(|p| p.commit).map(String::from),
        Some(COMMIT.to_owned())
    );
    assert_eq!(read.beat.version, Version::parse("1.0").expect("a version"));
}

/// Catches: a commit that is not the 40 lowercase hex digits a release is kept under.
#[test]
fn a_commit_is_forty_lowercase_hex_digits() {
    assert_eq!(
        Commit::parse(COMMIT).map(|c| c.to_string()),
        Some(COMMIT.to_owned())
    );
    assert_eq!(
        Commit::parse(COMMIT).map(String::from).as_deref(),
        Some(COMMIT)
    );
    for bad in [
        "",
        &COMMIT[..39],
        &format!("{COMMIT}0"),
        &COMMIT.to_uppercase(),
        &format!("g{}", &COMMIT[1..]),
    ] {
        assert_eq!(Commit::parse(bad), None, "{bad:?}");
        assert_eq!(
            Commit::try_from(bad.to_owned()),
            Err("not a commit: 40 lowercase hex digits")
        );
    }
}
