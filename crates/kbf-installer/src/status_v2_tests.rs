use serde_json::{Value, json};

use super::*;
use crate::golden::{STATUS, status};

/// Catches a change to `status.json` that `kbf-update` would misread and that did not move
/// the golden file.
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

/// Catches a status reader that takes a schema 1 file, an unknown key, or a word that is not
/// a refusal.
#[test]
fn a_foreign_status_is_refused() {
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v: Value = serde_json::from_str(STATUS).expect("json");
        f(&mut v);
        serde_json::to_vec(&v).expect("bytes")
    };
    assert!(StatusV2::parse(&edit(&|v| v["schema"] = json!(1))).is_err());
    assert!(StatusV2::parse(&edit(&|v| v["note"] = json!("x"))).is_err());
    assert!(StatusV2::parse(&edit(&|v| v["refusal"]["word"] = json!("busy"))).is_err());
    assert!(StatusV2::parse(&edit(&|v| v["state"] = json!("rebooting"))).is_err());
}
