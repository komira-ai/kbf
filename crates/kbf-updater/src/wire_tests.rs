use serde_json::json;

use crate::wire::{Health, Heartbeat, Outcome, Reply, UpdateResult};
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
        last_update: Some(UpdateResult {
            target: v("0.1.8+gabcdefabcdef-1"),
            outcome: Outcome::Failed,
        }),
        updating: Some(v("0.1.9")),
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
