//! The clear generation of an answer (`halts`): written only when there is one, read by
//! the agent that announced `halt-v1`, and refused by the readers that predate it.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::wire::{Answer, MAX_SERVICES, ServiceReply};

/// The answer reader of an agent that predates `features` and `halts`: strict, as shipped.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FrozenReply {
    reply: String,
    service: Option<String>,
    version: Option<String>,
}

fn halts(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
    entries
        .iter()
        .map(|(name, generation)| ((*name).to_owned(), *generation))
        .collect()
}

fn answer(text: &str) -> Result<Answer, serde_json::Error> {
    serde_json::from_str(text)
}

/// Catches: `halts` written when it is empty, which would change the bytes every agent reads
/// today: an answer with no generation is the reply alone, or the reply and `features`, as it
/// was before the field existed.
#[test]
fn an_answer_without_halts_is_written_as_it_was() {
    let plain = Answer::of(ServiceReply::Nothing);
    assert_eq!(
        serde_json::to_string(&plain).expect("json"),
        r#"{"reply":"nothing"}"#
    );
    let featured = Answer {
        features: Some(vec!["halt-v1".to_owned()]),
        ..plain
    };
    assert_eq!(
        serde_json::to_string(&featured).expect("json"),
        r#"{"reply":"nothing","features":["halt-v1"]}"#
    );
}

/// Catches: the generations lost, misplaced or unsorted on the way out: they follow `features`,
/// by service name, and are read back as written.
#[test]
fn halts_are_written_after_features_and_read_back() {
    let sent = Answer {
        features: Some(vec!["halt-v1".to_owned()]),
        halts: halts(&[("other", 2), ("kbf", 1)]),
        ..Answer::of(ServiceReply::Nothing)
    };
    let text = serde_json::to_string(&sent).expect("json");
    assert_eq!(
        text,
        r#"{"reply":"nothing","features":["halt-v1"],"halts":{"kbf":1,"other":2}}"#
    );
    assert_eq!(answer(&text).expect("read"), sent);
}

/// Catches: a new agent refusing the answer of a coordinator that predates `halts`: an answer
/// without the key reads with none, whether it carries `features` or not.
#[test]
fn an_answer_without_halts_reads_with_none() {
    for text in [
        r#"{"reply":"nothing"}"#,
        r#"{"reply":"nothing","features":["halt-v1"]}"#,
    ] {
        assert!(answer(text).expect("read").halts.is_empty(), "{text}");
    }
}

/// Catches: a generation that is not a service's or not a number being taken into the
/// agent's state: a bad name, a negative or fractional or textual generation, a list in place
/// of the map and more entries than a coordinator has services (`kbf` and a full services
/// file) are each refused, and that many are read.
#[test]
fn a_bad_halts_map_is_refused() {
    let map = |n: usize| -> String {
        let halts: BTreeMap<String, u64> = (0..n).map(|n| (format!("s{n}"), 1)).collect();
        serde_json::json!({"reply": "nothing", "halts": halts}).to_string()
    };
    assert_eq!(
        answer(&map(MAX_SERVICES + 1))
            .expect("at the cap")
            .halts
            .len(),
        MAX_SERVICES + 1
    );
    let too_many = map(MAX_SERVICES + 2);
    for text in [
        r#"{"reply":"nothing","halts":{"Kbf":1}}"#,
        r#"{"reply":"nothing","halts":{"":1}}"#,
        r#"{"reply":"nothing","halts":{"kbf":-1}}"#,
        r#"{"reply":"nothing","halts":{"kbf":1.5}}"#,
        r#"{"reply":"nothing","halts":{"kbf":"1"}}"#,
        r#"{"reply":"nothing","halts":["kbf"]}"#,
        &too_many,
    ] {
        assert!(answer(text).is_err(), "{text}");
    }
}

/// Catches: the frozen readers being loosened to take the field: an agent that never announced
/// `halt-v1` refuses an answer with `halts`, which is why the coordinator sends it only to one
/// that did.
#[test]
fn the_readers_that_predate_halts_refuse_it() {
    let text = r#"{"reply":"nothing","halts":{"kbf":1}}"#;
    assert!(serde_json::from_str::<FrozenReply>(text).is_err());
    assert!(serde_json::from_str::<ServiceReply>(text).is_err());
    assert!(serde_json::from_str::<crate::wire::Reply>(text).is_err());
}

/// Catches: the answer's reply being altered by the generations it travels with: the reply of
/// an answer read with `halts` is the reply alone, and comparing with one ignores them.
#[test]
fn halts_do_not_change_the_reply() {
    let text = r#"{"reply":"update","service":"other","version":"3.0","halts":{"kbf":1}}"#;
    let read = answer(text).expect("read");
    let want: ServiceReply =
        serde_json::from_str(r#"{"reply":"update","service":"other","version":"3.0"}"#)
            .expect("a reply");
    assert_eq!(read, want);
}
