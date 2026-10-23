use serde_json::{Value, json};

use super::*;
use crate::golden::{STATUS, status};

/// Catches a change to `status.json` that did not move the golden file or this type: the
/// golden bytes are the writer's (`helper/status_tests.rs`), and here they decode strictly to the
/// host they describe and encode back to the same bytes, key for key and in the same order.
///
/// Catches: a key of the type renamed or moved; a key the writer writes missing from the
/// type (the strict decode refuses the golden).
#[test]
fn the_golden_status_decodes_and_encodes_to_itself() {
    assert_eq!(
        StatusV2::parse(STATUS.as_bytes()).expect("parses"),
        status()
    );
    assert_eq!(
        String::from_utf8(status().render().expect("encodes")).expect("text"),
        STATUS
    );
}

/// Catches a host with no seed and no source encoded with the two keys as `null`, and a
/// status written before they existed refused: both are left out, and a file without them reads.
///
/// Catches: the `skip_serializing_if` of either key removed; the `default` of either
/// removed.
#[test]
fn a_status_without_a_seed_or_a_source_has_neither_key_and_still_reads() {
    let bare = StatusV2 {
        release_source: None,
        available: None,
        ..status()
    };
    let bytes = bare.render().expect("encodes");
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    for key in ["release_source", "available", "unverified"] {
        assert!(json.get(key).is_none(), "{key}");
    }
    assert_eq!(StatusV2::parse(&bytes).expect("parses"), bare);
}

/// Catches a renamed, added or dropped refusal word: scripts and the frontend branch on them
/// (contract section 5), so the list is fixed here word for word.
#[test]
fn the_refusal_words_are_the_contracts() {
    let words: Vec<String> = Refusal::ALL
        .iter()
        .map(|r| {
            serde_json::to_value(r)
                .expect("value")
                .as_str()
                .expect("str")
                .to_owned()
        })
        .collect();
    assert_eq!(
        words,
        [
            "trust_chain",
            "hash",
            "attestation",
            "commit_or_ref",
            "facts",
            "installer_too_old",
            "platform",
            "size",
            "disk",
            "fetch",
            "replay",
            "generation",
            "floor",
            "pending",
            "lease",
            "denied",
            "envoy_range",
            "frontend_busy",
        ]
    );
}

/// Catches a state word that no longer maps 1:1 to `UpdateStatus.State`.
#[test]
fn the_state_words_are_fixed() {
    let all = [
        (Phase::Idle, "idle"),
        (Phase::Fetching, "fetching"),
        (Phase::Staged, "staged"),
        (Phase::ActivatedUnconfirmed, "activated_unconfirmed"),
        (Phase::Confirmed, "confirmed"),
        (Phase::Reverted, "reverted"),
        (Phase::Refused, "refused"),
    ];
    for (phase, word) in all {
        assert_eq!(serde_json::to_value(phase).expect("value"), json!(word));
    }
}

/// Catches a status reader that takes a schema 1 file, an unknown key (also inside a nested
/// object), or a state word that is not one.
#[test]
fn a_foreign_status_is_refused() {
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v: Value = serde_json::from_str(STATUS).expect("json");
        f(&mut v);
        serde_json::to_vec(&v).expect("bytes")
    };
    assert!(StatusV2::parse(&edit(&|v| v["schema"] = json!(1))).is_err());
    assert!(StatusV2::parse(&edit(&|v| v["note"] = json!("x"))).is_err());
    assert!(StatusV2::parse(&edit(&|v| v["available"]["facts"] = json!(1))).is_err());
    assert!(StatusV2::parse(&edit(&|v| v["state"] = json!("rebooting"))).is_err());
}
