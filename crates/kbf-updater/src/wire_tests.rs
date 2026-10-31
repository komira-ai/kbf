use serde_json::json;

use crate::wire::{
    Health, Heartbeat, MAX_SERVICES, Outcome, Reply, ServiceReply, UpdateResult, read_heartbeat,
};
use kbf_types::ReleasePlatform;

fn v(text: &str) -> crate::version::Version {
    crate::version::Version::parse(text).expect("a version")
}

/// Catches: a wire spelling that drifted from the design's table (a field renamed, the
/// platform spelled other than the release file names), and a heartbeat that does not
/// read back as written, with and without the optional fields.
#[test]
fn a_heartbeat_is_written_and_read_in_one_spelling() {
    let busy = Heartbeat {
        platform: ReleasePlatform::DarwinArm64,
        version: v("0.1.7+g0123456789ab-3"),
        health: Health::Unhealthy,
        last_update: Some(UpdateResult::new(
            v("0.1.8+gabcdefabcdef-1"),
            Outcome::Failed,
        )),
        updating: Some(v("0.1.9")),
        services: None,
        agent_features: Vec::new(),
        step: None,
        commit: None,
        step_secs: Default::default(),
        promotion_pending: false,
        deferred: None,
    };
    assert_eq!(
        serde_json::to_value(&busy).expect("json"),
        json!({
            "platform": "darwin-arm64",
            "version": "0.1.7+g0123456789ab-3",
            "health": "unhealthy",
            "last_update": {"target": "0.1.8+gabcdefabcdef-1", "outcome": "failed"},
            "updating": "0.1.9",
        })
    );
    let plain = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy"}"#;
    let read: Heartbeat = serde_json::from_str(plain).expect("optional fields may be absent");
    assert_eq!(read.last_update, None);
    assert_eq!(read.updating, None);
    assert_eq!(read.platform, ReleasePlatform::LinuxX86_64);
    for beat in [busy, read] {
        let text = serde_json::to_string(&beat).expect("json");
        assert_eq!(
            serde_json::from_str::<Heartbeat>(&text).expect("back"),
            beat
        );
    }
    let ok = r#"{"platform":"linux-arm64","version":"1.0","health":"healthy","outcome":"x"}"#;
    assert!(
        serde_json::from_str::<Heartbeat>(ok).is_ok(),
        "unknown fields are ignored"
    );
}

/// Catches: a heartbeat accepted with a version the host-side rule refuses, a platform no
/// release is built for, a health word that is not one, or a missing required field.
#[test]
fn a_heartbeat_with_an_unusable_field_is_refused() {
    let good = json!({"platform": "linux-arm64", "version": "1.0", "health": "healthy"});
    assert!(serde_json::from_value::<Heartbeat>(good.clone()).is_ok());
    for (field, bad) in [
        ("version", json!("-1")),
        ("version", json!("1.0\n")),
        ("version", json!("")),
        ("platform", json!("macos-arm64")),
        ("platform", json!("LINUX-ARM64")),
        ("platform", json!(5)),
        ("health", json!("fine")),
        ("updating", json!("a/b")),
        ("last_update", json!({"target": "1.0", "outcome": "done"})),
    ] {
        let mut beat = good.clone();
        beat[field] = bad.clone();
        assert!(
            serde_json::from_value::<Heartbeat>(beat).is_err(),
            "{field} = {bad}"
        );
    }
    for field in ["platform", "version", "health"] {
        let mut beat = good.clone();
        beat.as_object_mut().expect("object").remove(field);
        assert!(
            serde_json::from_value::<Heartbeat>(beat).is_err(),
            "no {field}"
        );
    }
}

/// Catches: a reply that carries anything but a version: the two replies are written with
/// exactly the keys `reply` and (for an update) `version`, read back unchanged, and a reply
/// with a URL, a path or any other field is refused by the reader.
#[test]
fn a_reply_is_nothing_or_a_version_and_no_more() {
    assert_eq!(
        serde_json::to_value(Reply::Nothing).expect("json"),
        json!({"reply": "nothing"})
    );
    let update = Reply::Update {
        version: v("1.2.3-1"),
    };
    assert_eq!(
        serde_json::to_value(&update).expect("json"),
        json!({"reply": "update", "version": "1.2.3-1"})
    );
    for reply in [Reply::Nothing, update] {
        let text = serde_json::to_string(&reply).expect("json");
        assert_eq!(serde_json::from_str::<Reply>(&text).expect("back"), reply);
    }
    for bad in [
        r#"{"reply":"update","version":"1.0","url":"https://x.example/kbf"}"#,
        r#"{"reply":"update","version":"1.0","path":"/tmp/kbf"}"#,
        r#"{"reply":"nothing","version":"1.0"}"#,
        r#"{"reply":"update"}"#,
        r#"{"reply":"update","version":"https://x.example/kbf"}"#,
        r#"{"reply":"update","version":"../1.0"}"#,
        r#"{"reply":"reboot"}"#,
        r#"{}"#,
    ] {
        assert!(serde_json::from_str::<Reply>(bad).is_err(), "{bad}");
    }
}

const NEW_BEAT: &str = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":[{"name":"other","version":"2.0","health":"unhealthy","last_update":{"target":"2.0","outcome":"failed"},"updating":"3.0"},{"name":"third","version":"1.1","health":"healthy"}]}"#;

/// Behaviour: a heartbeat with `services` reads each entry's four facts and writes back
/// the same bytes; one without writes no `services` key at all (so an old updater's bytes are
/// unchanged), and an empty list is kept as a list (the updater knows services).
#[test]
fn a_heartbeat_carries_its_services_and_only_when_it_has_them() {
    let read: Heartbeat = serde_json::from_str(NEW_BEAT).expect("a new heartbeat");
    let services = read.services.as_ref().expect("services");
    assert_eq!(services.len(), 2);
    assert_eq!(services[0].name, "other");
    assert_eq!(services[0].health, Health::Unhealthy);
    assert_eq!(services[0].updating, Some(v("3.0")));
    assert_eq!(services[1].last_update, None);
    let platform = services[0].as_heartbeat(read.platform);
    assert_eq!(platform.version, v("2.0"));
    assert_eq!(platform.services, None);
    assert_eq!(
        serde_json::to_value(&read).expect("json"),
        serde_json::from_str::<serde_json::Value>(NEW_BEAT)
            .map(|mut j| {
                for s in j["services"].as_array_mut().expect("list") {
                    s.as_object_mut()
                        .expect("object")
                        .entry("last_update")
                        .or_insert(json!(null));
                    s.as_object_mut()
                        .expect("object")
                        .entry("updating")
                        .or_insert(json!(null));
                }
                j["last_update"] = json!(null);
                j["updating"] = json!(null);
                j
            })
            .expect("json")
    );
    let old = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy"}"#;
    let old: Heartbeat = serde_json::from_str(old).expect("old");
    assert_eq!(old.services, None);
    assert!(
        serde_json::to_value(&old)
            .expect("json")
            .get("services")
            .is_none()
    );
    let empty = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":[]}"#;
    let empty: Heartbeat = serde_json::from_str(empty).expect("empty");
    assert_eq!(empty.services, Some(Vec::new()));
}

/// Behaviour: a heartbeat that lists kbf (it is the top level), lists a service twice, or
/// names a service with a character a name cannot have, or none, is not read as a `Heartbeat`.
#[test]
fn a_heartbeat_with_a_bad_services_list_is_refused() {
    let entry = |name: &str| format!(r#"{{"name":"{name}","version":"1.0","health":"healthy"}}"#);
    let long = "a".repeat(65);
    for lists in [
        vec![entry("kbf")],
        vec![entry("other"), entry("other")],
        vec![entry("Other")],
        vec![entry("")],
        vec![entry("a b")],
        vec![entry(&long)],
    ] {
        let text = format!(
            r#"{{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":[{}]}}"#,
            lists.join(",")
        );
        assert!(serde_json::from_str::<Heartbeat>(&text).is_err(), "{text}");
    }
    let null = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":null}"#;
    assert_eq!(
        serde_json::from_str::<Heartbeat>(null)
            .expect("null")
            .services,
        None
    );
}

/// Behaviour: the reply of an updater that announced services. It is written as `Reply`
/// writes it when it names no service, with `service` when it does, and read back; the
/// reader refuses a bad service name, a service on `nothing`, a missing version and unknown
/// fields.
#[test]
fn a_service_reply_is_a_reply_that_may_name_a_service() {
    for (reply, text) in [
        (ServiceReply::Nothing, r#"{"reply":"nothing"}"#),
        (
            ServiceReply::Update {
                service: None,
                version: v("1.0"),
            },
            r#"{"reply":"update","version":"1.0"}"#,
        ),
        (
            ServiceReply::Update {
                service: Some("other".to_owned()),
                version: v("1.0"),
            },
            r#"{"reply":"update","service":"other","version":"1.0"}"#,
        ),
    ] {
        assert_eq!(serde_json::to_string(&reply).expect("json"), text);
        assert_eq!(
            serde_json::from_str::<ServiceReply>(text).expect("back"),
            reply
        );
    }
    for bad in [
        r#"{"reply":"update","service":"Other","version":"1.0"}"#,
        r#"{"reply":"nothing","service":"other"}"#,
        r#"{"reply":"nothing","version":"1.0"}"#,
        r#"{"reply":"update","service":"other"}"#,
        r#"{"reply":"update","version":"1.0","url":"https://x.example"}"#,
        r#"{"reply":"reboot"}"#,
    ] {
        assert!(serde_json::from_str::<ServiceReply>(bad).is_err(), "{bad}");
    }
    let old = Reply::Update { version: v("1.0") };
    assert_eq!(ServiceReply::from(old.clone()), old);
    assert_ne!(ServiceReply::Nothing, old);
}

/// Behaviour (the safety rule): the reader of today, [`Reply`], refuses a reply that names a
/// service, so the coordinator must never send one to an updater that did not announce
/// services; and it still reads the old shape that `ServiceReply` writes for kbf.
#[test]
fn the_old_reply_reader_refuses_a_service_reply() {
    let named = r#"{"reply":"update","service":"other","version":"1.0"}"#;
    assert!(serde_json::from_str::<Reply>(named).is_err());
    assert!(serde_json::from_str::<ServiceReply>(named).is_ok());
    let kbf = serde_json::to_string(&ServiceReply::from(Reply::Update { version: v("1.0") }))
        .expect("json");
    assert_eq!(
        serde_json::from_str::<Reply>(&kbf).expect("old reader"),
        Reply::Update { version: v("1.0") }
    );
}

/// Behaviour: a coordinator built before `services` existed still reads a new heartbeat:
/// its struct (the old fields, as the type was) ignores the unknown key.
#[test]
fn a_heartbeat_with_services_is_read_by_the_struct_before_services() {
    #[derive(serde::Deserialize)]
    struct Before {
        platform: String,
        version: crate::version::Version,
        health: Health,
        last_update: Option<UpdateResult>,
        updating: Option<crate::version::Version>,
    }
    let read: Before = serde_json::from_str(NEW_BEAT).expect("tolerated");
    assert_eq!(read.platform, "linux-x86_64");
    assert_eq!(read.version, v("1.0"));
    assert_eq!(read.health, Health::Healthy);
    assert_eq!(read.last_update, None);
    assert_eq!(read.updating, None);
}

fn beat_with(names: &[String]) -> String {
    let entries: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"name":"{n}","version":"1.0","health":"healthy"}}"#))
        .collect();
    format!(
        r#"{{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":[{}]}}"#,
        entries.join(",")
    )
}

fn numbered(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("svc-{i}")).collect()
}

/// Catches: a cap on the services list that is off by one, or missing: a list of exactly
/// `MAX_SERVICES` is read, one more is not.
#[test]
fn a_heartbeat_lists_at_most_the_most_services() {
    let at = beat_with(&numbered(MAX_SERVICES));
    let read: Heartbeat = serde_json::from_str(&at).expect("at the cap");
    assert_eq!(read.services.map(|s| s.len()), Some(MAX_SERVICES));
    let over = beat_with(&numbered(MAX_SERVICES + 1));
    assert!(serde_json::from_str::<Heartbeat>(&over).is_err());
}

/// Catches: a host with a bad services list losing its kbf updates: `read_heartbeat` reads
/// a good heartbeat as it is, and for each bad list (kbf listed, a name twice, a bad name,
/// over the cap, an entry that is no service, a list that is no list) keeps the rest of the
/// heartbeat with `services` dropped and says why.
#[test]
fn a_bad_services_list_is_dropped_and_the_rest_is_kept() {
    let good = read_heartbeat(NEW_BEAT.as_bytes()).expect("good");
    assert_eq!(good.services_dropped, None);
    assert_eq!(good.beat.services.as_ref().map(Vec::len), Some(2));
    let strict: Heartbeat = serde_json::from_str(NEW_BEAT).expect("strict");
    assert_eq!(good.beat, strict);
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let mut bad = vec![
        beat_with(&names(&["kbf"])),
        beat_with(&names(&["other", "other"])),
        beat_with(&names(&["Other"])),
        beat_with(&numbered(MAX_SERVICES + 1)),
        r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":[3]}"#
            .to_owned(),
        r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy","services":"x"}"#
            .to_owned(),
    ];
    bad.push(bad[0].replace(
        r#""health":"healthy","services""#,
        r#""health":"healthy","updating":"2.0","services""#,
    ));
    for text in &bad {
        let read = read_heartbeat(text.as_bytes()).expect(text);
        assert_eq!(read.beat.services, None, "{text}");
        assert_eq!(read.beat.version, v("1.0"));
        assert!(read.services_dropped.is_some(), "{text}");
    }
    let kept = read_heartbeat(bad.last().expect("last").as_bytes()).expect("kept");
    assert_eq!(kept.beat.updating, Some(v("2.0")));
}

/// Catches: the lenient read accepting what is wrong beyond the services list: not JSON,
/// not an object, a missing, bad or duplicated field, with or without a services key, is an error.
#[test]
fn a_heartbeat_that_is_wrong_beyond_its_services_is_still_refused() {
    for text in [
        "not json",
        "[]",
        "{}",
        r#"{"platform":"linux-x86_64","health":"healthy"}"#,
        r#"{"platform":"linux-x86_64","health":"healthy","services":[]}"#,
        r#"{"platform":"linux-x86_64","version":"1","version":"2","health":"healthy","services":[]}"#,
        r#"{"platform":"linux-x86_64","version":"1","health":"healthy","services":3,"services":[]}"#,
        r#"{"platform":"linux-x86_64","version":"-1","health":"healthy","services":3}"#,
    ] {
        assert!(read_heartbeat(text.as_bytes()).is_err(), "{text}");
    }
}
