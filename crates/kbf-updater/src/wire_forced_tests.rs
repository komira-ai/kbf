//! The `forced` flag of an answer: written only when it is true, read by the agent that
//! announced `forced-v1`, and refused by the readers that predate it.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::version::Version;
use crate::wire::{Answer, ServiceReply};

/// The answer reader of the release before `forced`: `features` and `halts`, strict, as
/// shipped.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // The fields are read only to be refused or accepted, never used.
struct FrozenHaltAnswer {
    reply: String,
    service: Option<String>,
    version: Option<String>,
    features: Option<Vec<String>>,
    #[serde(default)]
    halts: BTreeMap<String, u64>,
}

fn update(version: &str) -> ServiceReply {
    ServiceReply::Update {
        service: None,
        version: Version::parse(version).expect("version"),
    }
}

/// Catches: `forced` written when it is false, which would change the bytes every agent reads
/// today, or written under another name or place: it follows the other keys, and only when
/// true.
#[test]
fn forced_is_written_after_the_rest_and_only_when_true() {
    let plain = Answer {
        features: Some(vec!["forced-v1".to_owned()]),
        ..Answer::of(update("1.0"))
    };
    let text = serde_json::to_string(&plain).expect("json");
    assert_eq!(
        text,
        r#"{"reply":"update","version":"1.0","features":["forced-v1"]}"#
    );
    let forced = Answer {
        forced: true,
        halts: BTreeMap::from([("kbf".to_owned(), 3)]),
        ..plain
    };
    let text = serde_json::to_string(&forced).expect("json");
    assert_eq!(
        text,
        r#"{"reply":"update","version":"1.0","features":["forced-v1"],"halts":{"kbf":3},"forced":true}"#
    );
    assert_eq!(serde_json::from_str::<Answer>(&text).expect("read"), forced);
}

/// Catches: a new agent refusing the answer of a coordinator that predates `forced`, or reading
/// its update as forced: an answer without the key, or with it false, is an ordinary order.
#[test]
fn an_answer_without_forced_is_an_ordinary_order() {
    for text in [
        r#"{"reply":"update","version":"1.0","features":[]}"#,
        r#"{"reply":"update","version":"1.0","forced":false}"#,
    ] {
        let read: Answer = serde_json::from_str(text).expect("read");
        assert!(!read.forced, "{text}");
        assert_eq!(read, update("1.0"));
    }
}

/// Catches: a forced answer that orders nothing being read as something: `forced` is about an
/// update, and a reply of nothing that carries it is no answer any coordinator sends.
#[test]
fn a_forced_reply_of_nothing_is_refused() {
    let text = r#"{"reply":"nothing","forced":true}"#;
    let err = serde_json::from_str::<Answer>(text).expect_err("refused");
    assert_eq!(err.to_string(), "a reply of nothing is not forced");
    for text in [
        r#"{"reply":"update","version":"1.0","forced":1}"#,
        r#"{"reply":"update","version":"1.0","forced":"yes"}"#,
    ] {
        assert!(serde_json::from_str::<Answer>(text).is_err(), "{text}");
    }
}

/// Catches: `forced` sent to the agent of the release before it, whose strict reader refuses
/// the whole answer: that reader takes an ordinary answer and refuses a forced one, which is
/// why the coordinator sends the flag only to an agent that announced `forced-v1`.
#[test]
fn the_release_before_reads_an_ordinary_answer_and_refuses_a_forced_one() {
    let ordinary = Answer {
        features: Some(vec!["halt-v1".to_owned()]),
        halts: BTreeMap::from([("kbf".to_owned(), 1)]),
        ..Answer::of(update("1.0"))
    };
    let text = serde_json::to_string(&ordinary).expect("json");
    assert!(serde_json::from_str::<FrozenHaltAnswer>(&text).is_ok());
    let forced = Answer {
        forced: true,
        ..ordinary
    };
    let text = serde_json::to_string(&forced).expect("json");
    assert!(serde_json::from_str::<FrozenHaltAnswer>(&text).is_err());
}
