//! The optional facts of a heartbeat and the features of an answer: what an old reader does
//! with them, what a new one refuses, and what the coordinator's reader drops.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::version::Version;
use crate::wire::{
    Answer, Class, CommitState, Heartbeat, Outcome, Phase, Reply, ServiceReply, Token,
    UpdateResult, read_heartbeat,
};

fn v(text: &str) -> Version {
    Version::parse(text).expect("a version")
}

/// The heartbeat of v0.0.18, as its updater writes it.
const OLD_BEAT: &str = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","last_update":{"target":"1.1","outcome":"failed"},"updating":null}"#;

/// A heartbeat that uses every new field, on kbf and on one service.
fn full_beat() -> Value {
    let facts = json!({
        "step": {"name": "verify", "attempt": 2},
        "commit": "committed",
        "step_secs": {"prepare": 3, "cutover": 41},
        "promotion_pending": true,
        "deferred": {"target": "1.2", "reason": "drain_bound", "retry_after_secs": 300},
    });
    let mut service = json!({"name": "db", "version": "1.0", "health": "healthy",
        "last_update": null, "updating": null});
    let mut top = json!({
        "platform": "linux-x86_64", "version": "1.0", "health": "unhealthy",
        "last_update": {"target": "1.1", "outcome": "failed", "phase": "halt",
                        "class": "environment", "reason": "state_invalid"},
        "updating": null,
        "agent_features": ["deferred-v1", "halt-v1"],
    });
    for (key, value) in facts.as_object().expect("an object") {
        service[key] = value.clone();
        top[key] = value.clone();
    }
    top["services"] = json!([service]);
    top
}

fn read(value: &Value) -> Result<Heartbeat, serde_json::Error> {
    serde_json::from_slice(&serde_json::to_vec(value).expect("json"))
}

/// Catches: a new field that is required, or written when it is empty: the heartbeat of an
/// updater that knows none of them is read as before and written back byte for byte, so the
/// bytes v0.0.18 and the goldens pin do not move.
#[test]
fn a_heartbeat_without_the_new_fields_is_read_and_written_as_it_was() {
    let beat: Heartbeat = serde_json::from_str(OLD_BEAT).expect("an old heartbeat");
    assert_eq!(beat.agent_features, Vec::<String>::new());
    assert_eq!(beat.step, None);
    assert_eq!(beat.commit, None);
    assert!(beat.step_secs.is_empty());
    assert!(!beat.promotion_pending);
    assert_eq!(beat.deferred, None);
    assert_eq!(serde_json::to_string(&beat).expect("json"), OLD_BEAT);
}

/// Catches: a field renamed, a value spelled another way, or one dropped on the way through:
/// every new field is read from its name, and the heartbeat is written back as it came.
#[test]
fn a_heartbeat_with_every_new_field_is_read_and_written_back() {
    let wire = full_beat();
    let beat = read(&wire).expect("a heartbeat");
    assert_eq!(beat.agent_features, ["deferred-v1", "halt-v1"]);
    let step = beat.step.as_ref().expect("a step");
    assert_eq!((step.name.as_str(), step.attempt), ("verify", 2));
    assert_eq!(beat.commit, Some(CommitState::Committed));
    assert_eq!(beat.step_secs.len(), 2);
    assert!(beat.promotion_pending);
    let deferred = beat.deferred.as_ref().expect("deferred");
    assert_eq!(deferred.target, v("1.2"));
    assert_eq!(deferred.reason.as_str(), "drain_bound");
    assert_eq!(deferred.retry_after_secs, 300);
    let last = beat.last_update.as_ref().expect("a last update");
    assert_eq!(last.phase, Some(Phase::Halt));
    assert_eq!(last.class, Some(Class::Environment));
    assert_eq!(
        last.reason.as_ref().map(Token::as_str),
        Some("state_invalid")
    );
    let service = &beat.services.as_ref().expect("services")[0];
    assert!(service.promotion_pending);
    assert_eq!(service.commit, Some(CommitState::Committed));
    assert_eq!(serde_json::to_value(&beat).expect("json"), wire);
}

/// The heartbeat struct of v0.0.18, frozen: no `deny_unknown_fields`, no new field.
#[derive(Deserialize)]
#[allow(dead_code)]
struct FrozenHeartbeat {
    platform: String,
    version: String,
    health: String,
    #[serde(default)]
    last_update: Option<FrozenUpdate>,
    #[serde(default)]
    updating: Option<String>,
    #[serde(default)]
    services: Option<Vec<FrozenService>>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct FrozenUpdate {
    target: String,
    outcome: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct FrozenService {
    name: String,
    version: String,
    health: String,
    #[serde(default)]
    last_update: Option<FrozenUpdate>,
    #[serde(default)]
    updating: Option<String>,
}

/// Catches: a new field an old coordinator cannot read past: the frozen v0 reader parses a
/// heartbeat that carries every new field, on kbf, on a service and in `last_update`.
#[test]
fn a_heartbeat_with_every_new_field_is_read_by_the_frozen_v0_reader() {
    let bytes = serde_json::to_vec(&full_beat()).expect("json");
    let old: FrozenHeartbeat = serde_json::from_slice(&bytes).expect("the old reader reads it");
    assert_eq!(old.version, "1.0");
    assert_eq!(old.last_update.expect("last update").outcome, "failed");
    assert_eq!(old.services.expect("services")[0].name, "db");
}

/// Catches: a phase, class or reason spelled other than the design's and the hook contract's.
#[test]
fn the_phase_class_and_reason_are_spelled_as_the_contract_spells_them() {
    for (text, phase) in [
        ("before_commit", Phase::BeforeCommit),
        ("gate_release", Phase::GateRelease),
        ("gate_environment", Phase::GateEnvironment),
        ("after_commit", Phase::AfterCommit),
        ("halt", Phase::Halt),
    ] {
        assert_eq!(serde_json::to_value(phase).expect("json"), json!(text));
        assert_eq!(
            serde_json::from_value::<Phase>(json!(text)).ok(),
            Some(phase)
        );
    }
    for (text, class) in [
        ("release", Class::Release),
        ("environment", Class::Environment),
    ] {
        assert_eq!(serde_json::to_value(class).expect("json"), json!(text));
        assert_eq!(
            serde_json::from_value::<Class>(json!(text)).ok(),
            Some(class)
        );
    }
    assert_eq!(
        serde_json::to_value(CommitState::Uncommitted).expect("json"),
        json!("uncommitted")
    );
}

/// Catches: a reason token that is longer, shorter or wider than `[a-z0-9_]`, 1 to 64 bytes.
#[test]
fn a_reason_is_a_token_of_one_to_sixty_four_bytes_of_lowercase_digits_and_underscore() {
    let longest = "a".repeat(64);
    for good in ["drain_bound", "a", "x9_", longest.as_str()] {
        assert!(Token::parse(good).is_some(), "{good}");
    }
    let too_long = "a".repeat(65);
    for bad in [
        "",
        "Drain",
        "drain-bound",
        "drain bound",
        "dräin",
        "a\n",
        too_long.as_str(),
    ] {
        assert!(Token::parse(bad).is_none(), "{bad:?}");
    }
}

/// Catches: a failure that costs the host its heartbeat because a newer agent spelled a phase,
/// a class or a reason in a way this reader does not know: those three read as absent, and the
/// result keeps its target and outcome.
#[test]
fn an_unknown_phase_class_or_reason_reads_as_absent_and_keeps_the_result() {
    let wire = json!({"target": "1.1", "outcome": "failed",
        "phase": "later_phase", "class": "cosmic", "reason": "Not A Token"});
    let result: UpdateResult = serde_json::from_value(wire).expect("a result");
    assert_eq!(result, UpdateResult::new(v("1.1"), Outcome::Failed));
}

/// Catches: a bad new field accepted by the strict read, one case at a time.
#[test]
fn the_strict_read_refuses_each_bad_new_field() {
    let nine: Vec<String> = (0..9).map(|i| format!("f{i}")).collect();
    let nine_steps: serde_json::Map<String, Value> =
        (0..9).map(|i| (format!("s{i}"), json!(1))).collect();
    let bad = [
        ("agent_features", json!(nine)),
        ("agent_features", json!(["a", "a"])),
        ("agent_features", json!(["a".repeat(33)])),
        ("agent_features", json!(["Upper"])),
        ("agent_features", json!([""])),
        ("step", json!({"name": "Verify", "attempt": 1})),
        ("step", json!({"name": "a".repeat(17), "attempt": 1})),
        ("step", json!({"name": "verify"})),
        ("commit", json!("maybe")),
        ("step_secs", json!(nine_steps)),
        ("step_secs", json!({"a".repeat(17): 1})),
        ("promotion_pending", json!("yes")),
        (
            "deferred",
            json!({"target": "1.2", "reason": "Busy", "retry_after_secs": 1}),
        ),
        (
            "deferred",
            json!({"target": "1.2", "reason": "x".repeat(65), "retry_after_secs": 1}),
        ),
        ("deferred", json!({"target": "1.2", "reason": "busy"})),
    ];
    for (key, value) in bad {
        let mut wire = full_beat();
        wire[key] = value.clone();
        assert!(read(&wire).is_err(), "{key}: {value}");
    }
}

/// Catches: a host losing its kbf updates, or the rest of its facts, to one bad optional
/// field: the coordinator's reader drops that field alone, says so, and keeps the others.
#[test]
fn a_bad_optional_field_is_dropped_alone_and_the_host_is_still_read() {
    let nine: Vec<String> = (0..9).map(|i| format!("f{i}")).collect();
    let bad = [
        ("agent_features", json!(nine)),
        ("agent_features", json!(["a", "a"])),
        ("agent_features", json!(["a".repeat(33)])),
        ("agent_features", json!(["Upper"])),
        ("agent_features", json!([""])),
        (
            "deferred",
            json!({"target": "1.2", "reason": "Busy", "retry_after_secs": 1}),
        ),
        ("step", json!({"name": "Verify", "attempt": 1})),
        ("commit", json!(7)),
        ("step_secs", json!({"A": 1})),
        ("promotion_pending", json!("yes")),
    ];
    let good = read(&full_beat()).expect("a heartbeat");
    for (key, value) in bad {
        let mut wire = full_beat();
        wire[key] = value.clone();
        let got = read_heartbeat(&serde_json::to_vec(&wire).expect("json")).expect("read");
        let why = got.extras_dropped.as_deref().unwrap_or_default();
        assert!(why.starts_with(key), "{key}: {why:?}");
        assert_eq!(got.services_dropped, None, "{key}");
        let mut expected = good.clone();
        match key {
            "agent_features" => expected.agent_features.clear(),
            "deferred" => expected.deferred = None,
            "step" => expected.step = None,
            "commit" => expected.commit = None,
            "step_secs" => expected.step_secs.clear(),
            _ => expected.promotion_pending = false,
        }
        assert_eq!(got.beat, expected, "{key}");
    }
}

/// Catches: the two droppable parts hiding each other: a bad `services` list and a bad new
/// field in one heartbeat are both dropped, each reported on its own, and kbf's own facts stay.
#[test]
fn a_bad_services_list_and_a_bad_new_field_are_both_dropped() {
    let mut wire = full_beat();
    wire["services"] = json!([{"name": "kbf", "version": "1.0", "health": "healthy"}]);
    wire["agent_features"] = json!(["Upper"]);
    let got = read_heartbeat(&serde_json::to_vec(&wire).expect("json")).expect("read");
    assert!(got.services_dropped.is_some());
    assert!(
        got.extras_dropped
            .is_some_and(|why| why.starts_with("agent_features"))
    );
    assert_eq!(got.beat.services, None);
    assert!(got.beat.promotion_pending);
    assert_eq!(got.beat.last_update.expect("last").phase, Some(Phase::Halt));
}

/// Catches: the lenient read excusing what is wrong with the heartbeat itself.
#[test]
fn a_heartbeat_that_is_wrong_beyond_its_new_fields_is_still_refused() {
    let mut wire = full_beat();
    wire["agent_features"] = json!(["Upper"]);
    wire["health"] = json!("sick");
    assert!(read_heartbeat(&serde_json::to_vec(&wire).expect("json")).is_err());
}

/// Catches: a good heartbeat reported as having dropped something.
#[test]
fn a_good_heartbeat_drops_nothing() {
    let got = read_heartbeat(&serde_json::to_vec(&full_beat()).expect("json")).expect("read");
    assert_eq!((got.services_dropped, got.extras_dropped), (None, None));
}

/// The two reply structs of v0.0.18 and of the services agent, frozen with their strict read.
mod frozen {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct Reply {
        pub reply: String,
        pub version: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct ServiceReply {
        pub reply: String,
        pub service: Option<String>,
        pub version: Option<String>,
    }
}

fn answers() -> Vec<ServiceReply> {
    vec![
        ServiceReply::Nothing,
        ServiceReply::Update {
            service: None,
            version: v("2.0"),
        },
        ServiceReply::Update {
            service: Some("db".to_owned()),
            version: v("2.0"),
        },
    ]
}

/// Catches: a `features` key reaching an agent that did not ask: an answer without features is
/// the bytes of the reply alone, and each frozen strict reader of the old agents parses it.
#[test]
fn an_answer_without_features_is_the_reply_and_an_old_agents_strict_reader_parses_it() {
    for reply in answers() {
        let bytes = serde_json::to_string(&Answer::of(reply.clone())).expect("json");
        assert_eq!(bytes, serde_json::to_string(&reply).expect("json"));
        assert!(!bytes.contains("features"));
        let strict: Result<frozen::ServiceReply, _> = serde_json::from_str(&bytes);
        assert!(strict.is_ok(), "{bytes}");
        if !matches!(
            &reply,
            ServiceReply::Update {
                service: Some(_),
                ..
            }
        ) {
            assert!(
                serde_json::from_str::<frozen::Reply>(&bytes).is_ok(),
                "{bytes}"
            );
            assert!(serde_json::from_str::<Reply>(&bytes).is_ok(), "{bytes}");
        }
    }
}

/// Pins why the gate exists: an old agent's strict reader refuses `features`, even empty.
#[test]
fn an_old_agents_strict_reader_refuses_a_reply_with_features() {
    for text in [
        r#"{"reply":"nothing","features":[]}"#,
        r#"{"reply":"update","version":"2.0","features":["x"]}"#,
    ] {
        assert!(serde_json::from_str::<frozen::Reply>(text).is_err());
        assert!(serde_json::from_str::<frozen::ServiceReply>(text).is_err());
        assert!(serde_json::from_str::<Reply>(text).is_err());
        assert!(serde_json::from_str::<ServiceReply>(text).is_err());
    }
}

/// Catches: `features` written under another name or position, or lost on the way back: the
/// reply's keys come first, `features` after them (an empty list is still written), and a new
/// agent's reader returns what was written.
#[test]
fn an_answer_with_features_names_them_after_the_reply_and_reads_back() {
    let cases = [
        (
            Answer {
                reply: ServiceReply::Nothing,
                features: Some(vec![]),
            },
            r#"{"reply":"nothing","features":[]}"#,
        ),
        (
            Answer {
                reply: ServiceReply::Update {
                    service: Some("db".to_owned()),
                    version: v("2.0"),
                },
                features: Some(vec!["deferred-v1".to_owned()]),
            },
            r#"{"reply":"update","service":"db","version":"2.0","features":["deferred-v1"]}"#,
        ),
    ];
    for (answer, text) in cases {
        assert_eq!(serde_json::to_string(&answer).expect("json"), text);
        assert_eq!(serde_json::from_str::<Answer>(text).expect("read"), answer);
    }
}

/// Catches: the answer reader accepting what no coordinator sends.
#[test]
fn the_answer_reader_refuses_an_unknown_key_a_bad_feature_and_a_bad_reply() {
    for text in [
        r#"{"reply":"nothing","other":1}"#,
        r#"{"reply":"nothing","features":["Upper"]}"#,
        r#"{"reply":"nothing","features":["a","a"]}"#,
        r#"{"reply":"nothing","version":"1.0"}"#,
        r#"{"reply":"update"}"#,
    ] {
        assert!(serde_json::from_str::<Answer>(text).is_err(), "{text}");
    }
}

/// Catches: an answer compared by its features: equality with a reply is about the reply.
#[test]
fn an_answer_equals_the_reply_it_carries() {
    let answer = Answer {
        reply: ServiceReply::Nothing,
        features: Some(vec!["x".to_owned()]),
    };
    assert_eq!(answer, ServiceReply::Nothing);
    assert_eq!(answer, Reply::Nothing);
    assert_ne!(answer, Reply::Update { version: v("1.0") });
}

/// Pins the byte budget of the new facts: an old coordinator refuses a heartbeat over
/// 16 KiB, so what an agent adds to one beat is bounded: all of them at their bounds add under
/// 900 bytes to a beat (seventeen beats, kbf and sixteen services, add under 15 KiB).
#[test]
fn the_new_facts_at_their_bounds_stay_inside_the_budget() {
    let token = "r".repeat(64);
    let step_secs: serde_json::Map<String, Value> = (0..8)
        .map(|i| (format!("{:_<16}", format!("s{i}")), json!(u64::MAX)))
        .collect();
    let facts = json!({
        "step": {"name": "n".repeat(16), "attempt": 4_294_967_295_u64},
        "commit": "uncommitted",
        "step_secs": step_secs,
        "promotion_pending": true,
        "deferred": {"target": "1".repeat(128), "reason": token, "retry_after_secs": u64::MAX},
    });
    let extras = |value: &Value| {
        let mut with = value.clone();
        for (key, fact) in facts.as_object().expect("an object") {
            with[key] = fact.clone();
        }
        with["last_update"]["phase"] = json!("gate_environment");
        with["last_update"]["class"] = json!("environment");
        with["last_update"]["reason"] = json!(token);
        serde_json::to_vec(&with).expect("json").len()
            - serde_json::to_vec(value).expect("json").len()
    };
    let beat = json!({"name": "n".repeat(64), "version": "1".repeat(128), "health": "healthy",
        "last_update": {"target": "1".repeat(128), "outcome": "failed"},
        "updating": "1".repeat(128)});
    assert!(17 * extras(&beat) < 15 * 1024);
    assert!(extras(&beat) < 900, "{}", extras(&beat));
}
