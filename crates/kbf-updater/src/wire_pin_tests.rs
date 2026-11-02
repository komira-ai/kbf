//! The heartbeat's `pinned`: its bytes, what the coordinator of the release before it reads of
//! it, what an agent of that release sends, and what this release's reader does with a bad
//! one, or with a good one beside a bad fact.

use std::collections::BTreeMap;

use kbf_types::ReleasePlatform;
use serde::{Deserialize, Serialize};

use super::{
    Commit, CommitState, DeferredReport, Health, Heartbeat, NotACommit, PinReport, ServiceBeat,
    StepName, StepRun, UpdateResult, features_text, is_false, or_default, platform_text,
    read_heartbeat, services_text, step_secs_text,
};
use crate::version::Version;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const BASE: &str = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":null,"updating":null"#;

/// `Heartbeat` as the release before the pin had it, word for word (its `ServiceBeat` and the
/// types of its fields are unchanged). The coordinator of that release reads a heartbeat with
/// it first (`read_heartbeat`'s strict read), so what it reads here, that coordinator reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReleasedHeartbeat {
    #[serde(with = "platform_text")]
    platform: ReleasePlatform,
    version: Version,
    health: Health,
    #[serde(default)]
    last_update: Option<UpdateResult>,
    #[serde(default)]
    updating: Option<Version>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "services_text"
    )]
    services: Option<Vec<ServiceBeat>>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "features_text"
    )]
    agent_features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step: Option<StepRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<CommitState>,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "step_secs_text"
    )]
    step_secs: BTreeMap<StepName, u64>,
    #[serde(
        default,
        skip_serializing_if = "is_false",
        deserialize_with = "or_default"
    )]
    promotion_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deferred: Option<DeferredReport>,
}

fn pin() -> Option<PinReport> {
    Some(PinReport {
        commit: Commit::parse(COMMIT),
    })
}

fn beat(pinned: Option<PinReport>) -> Heartbeat {
    let mut beat: Heartbeat = serde_json::from_str(&format!("{BASE}}}")).expect("a heartbeat");
    beat.pinned = pinned;
    beat
}

/// Pins the bytes of the new field: `pinned` is the last key, an object with the commit, and an
/// empty object for a pin whose commit is not known; a host that is not pinned writes none.
///
/// Catches: the field renamed, written when there is no pin, or its commit spelled otherwise.
#[test]
fn the_pinned_field_is_written_as_the_golden_says() {
    let pinned = beat(pin());
    assert_eq!(
        serde_json::to_string(&pinned).expect("json"),
        format!(r#"{BASE},"pinned":{{"commit":"{COMMIT}"}}}}"#)
    );
    let unknown = beat(Some(PinReport { commit: None }));
    assert_eq!(
        serde_json::to_string(&unknown).expect("json"),
        format!(r#"{BASE},"pinned":{{}}}}"#)
    );
    assert_eq!(
        serde_json::to_string(&beat(None)).expect("json"),
        format!("{BASE}}}")
    );
    for one in [pinned, unknown] {
        let text = serde_json::to_string(&one).expect("json");
        assert_eq!(serde_json::from_str::<Heartbeat>(&text).expect("read"), one);
    }
}

/// The released coordinator reads a pinned heartbeat, with every optional fact and a service,
/// as it reads the same heartbeat without the pin. And the other way: what a released agent
/// writes is read by this release as not pinned, every fact kept, and written back byte for
/// byte.
///
/// Catches: a pin that costs a host its heartbeat at a coordinator from before it; a field made
/// required.
#[test]
fn the_released_reader_takes_a_pinned_beat_and_this_reader_takes_a_released_one() {
    let full = format!(
        r#"{BASE},"services":[{{"name":"db","version":"2.0","health":"healthy","last_update":null,"updating":null}}],"agent_features":["deferred-v1","halt-v1"],"step":{{"name":"verify","attempt":2}},"commit":"committed","step_secs":{{"prepare":3}},"promotion_pending":true,"deferred":{{"target":"1.2","reason":"drain_bound","retry_after_secs":300}}"#
    );
    let mut ours: Heartbeat = serde_json::from_str(&format!("{full}}}")).expect("this release");
    ours.pinned = pin();
    let bytes = serde_json::to_vec(&ours).expect("json");
    let released: ReleasedHeartbeat = serde_json::from_slice(&bytes).expect("the released reader");
    let without: ReleasedHeartbeat =
        serde_json::from_str(&format!("{full}}}")).expect("the released reader");
    assert_eq!(released, without);

    let written = serde_json::to_string(&released).expect("json");
    let read = read_heartbeat(written.as_bytes()).expect("this reader");
    assert_eq!(read.beat.pinned, None);
    assert_eq!(read.extras_dropped, None);
    assert_eq!(read.services_dropped, None);
    assert_eq!(serde_json::to_string(&read.beat).expect("json"), written);
}

/// A `pinned` this reader cannot read (a commit that is not 40 hex digits, a value that is no
/// object) is dropped on its own and counted as `pinned`; the host keeps its heartbeat. A
/// `null` is no pin and `{}` a pin with no commit, neither of them dropped. A key a later agent
/// adds inside `pinned` is ignored.
///
/// Catches: a bad pin that makes the coordinator refuse the whole heartbeat; a pin refused for
/// a key it does not know.
#[test]
fn a_bad_pinned_is_dropped_and_null_empty_and_unknown_keys_are_read() {
    for bad in [r#""pinned":{"commit":"ABC"}"#, r#""pinned":7"#] {
        let read = read_heartbeat(format!("{BASE},{bad}}}").as_bytes()).expect("kept");
        assert_eq!(read.beat.pinned, None, "{bad}");
        assert_eq!(read.beat.version, Version::parse("1.0").expect("a version"));
        assert_eq!(read.extras_dropped_facts, ["pinned"], "{bad}");
        assert!(
            read.extras_dropped
                .as_deref()
                .is_some_and(|why| why.starts_with("pinned: ")),
            "{bad}"
        );
    }
    for (text, pinned) in [
        (r#""pinned":null"#, None),
        (r#""pinned":{}"#, Some(PinReport { commit: None })),
        (
            &*format!(r#""pinned":{{"commit":"{COMMIT}","by":"force","at_unix":9}}"#),
            pin(),
        ),
    ] {
        let read = read_heartbeat(format!("{BASE},{text}}}").as_bytes()).expect("read");
        assert_eq!(read.beat.pinned, pinned, "{text}");
        assert_eq!(read.extras_dropped, None, "{text}");
    }
}

/// A good pin survives the loose read that a bad optional fact, or a bad services list, sends
/// the heartbeat through: only the bad part is dropped.
///
/// Catches: a pin lost because another fact of the same heartbeat was bad, which would let the
/// coordinator order a pinned host.
#[test]
fn a_good_pin_survives_a_bad_fact_or_a_bad_services_list_beside_it() {
    let pinned = format!(r#""pinned":{{"commit":"{COMMIT}"}}"#);
    for (bad, facts, services_dropped) in [
        (r#""step":5"#, vec!["step"], false),
        (r#""deferred":"soon""#, vec!["deferred"], false),
        (
            r#""services":[{"name":"kbf","version":"1.0","health":"healthy"}]"#,
            vec![],
            true,
        ),
    ] {
        let read = read_heartbeat(format!("{BASE},{bad},{pinned}}}").as_bytes()).expect("kept");
        assert_eq!(read.beat.pinned, pin(), "{bad}");
        assert_eq!(read.extras_dropped_facts, facts, "{bad}");
        assert_eq!(read.services_dropped.is_some(), services_dropped, "{bad}");
        assert_eq!(read.beat.health, Health::Healthy);
    }
}

/// Catches: a commit that is not the 40 lowercase hex digits a release is kept under, and a
/// refusal that does not say what was given.
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
        let refused = Commit::try_from(bad.to_owned());
        assert_eq!(
            refused,
            Err(NotACommit {
                text: bad.to_owned()
            })
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            format!("not a commit: {bad:?} is not 40 lowercase hex digits")
        );
    }
}
