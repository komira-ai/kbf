//! `status.json` as the helper writes it: the golden bytes of the writer, and the node's reader
//! reading them. What each test proves is named on it, with the defect it catches.

use std::fs;

use kbf_update::Staged;
use kbf_update::helper::{Helper, HelperState, Report};

use super::*;
use crate::golden::STATUS;
use crate::helper::memory::StagedFacts;
use crate::seed::Available;
use crate::testkit::{A, B, C, c};

/// The host the golden file describes: running A, B staged and refused, C before A, a package
/// that carries a seed of `seq` 812 for B, and a source that fetches nothing.
fn described() -> (State, Memory) {
    let state = State {
        current: Some(c(A)),
        previous: Some(c(C)),
        staged: Some(c(B)),
        ..State::default()
    };
    let memory = Memory {
        acked_order_id: 41,
        event: Event::Refused,
        refusal: "attestation: no valid attestation for kbf at commit 0123456789ab".to_owned(),
        staged: Some(StagedFacts {
            commit: c(B),
            protocol: 12,
            min_protocol: 11,
            state_schema: 4,
        }),
        staged_profile: Some("release".to_owned()),
        release_source: Some("bundled"),
        available: Some(Available {
            commit: c(B),
            seq: 812,
        }),
        ..Memory::default()
    };
    (state, memory)
}

/// The writer's bytes for the host the golden file describes are the golden file: a change to the
/// shape of `status.json` (a key renamed, moved or added) moves the file in the same change, and
/// the diff of the file is the review of the format.
///
/// Catches a key of `status.json` changed without the readers being looked at: the node's
/// reader and this file both name the keys.
///
/// Catches: `available` written before `release_source`; either key written as `null`
/// when it is absent (the second test); `seq` written as text.
#[test]
fn the_golden_status_is_what_the_writer_writes() {
    let (state, memory) = described();
    let written = String::from_utf8(render(&state, &memory)).expect("text");
    assert_eq!(written, STATUS);
}

/// The node's reader reads what the writer writes, whole: the commits, the state word, the
/// refusal, the order acknowledged, the staged facts, and the two keys this change adds, the
/// seed that is available and the source the host fetches from.
///
/// Catches a writer and a reader that disagree on a key (the node would then report nothing of
/// the package to the frontend, silently), which the golden file alone cannot see.
///
/// Catches: the reader's `available` or `release_source` field renamed; the writer's
/// key renamed.
#[test]
fn the_node_reads_what_the_writer_writes() {
    let (state, memory) = described();
    let dir = kbf_host::testkit::scratch("status-node");
    let path = dir.join("status.json");
    fs::write(&path, render(&state, &memory)).expect("written");
    let want = Report {
        installed: Some(A.to_owned()),
        previous: Some(C.to_owned()),
        staged: Some(B.to_owned()),
        state: HelperState::Refused,
        refusal: "attestation: no valid attestation for kbf at commit 0123456789ab".to_owned(),
        acked_order_id: 41,
        staged_facts: Some(Staged {
            protocol: 12,
            min_protocol: 11,
            state_schema: 4,
        }),
        available: Some(kbf_update::Available {
            commit: B.to_owned(),
            seq: 812,
        }),
        release_source: kbf_update::ReleaseSource::Bundled,
    };
    assert_eq!(kbf_update::helper::read(&path), Helper::Reported(want));
}

/// A status of a host with no seed to announce and no source it could read carries neither key,
/// so that a status written before this change and one written after it are the same bytes on
/// such a host.
///
/// Catches a key written as `null` (a reader that treats a present key as a word would read a
/// source called "null").
///
/// Catches: the `skip_serializing_if` of either key removed.
#[test]
fn a_status_with_no_seed_and_no_source_carries_neither_key() {
    let bytes = render(&State::default(), &Memory::default());
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    for key in ["available", "release_source"] {
        assert!(json.get(key).is_none(), "{key}");
    }
}

/// Asserts that the id of the order a refusal answers is told while there is one, that the strict
/// reader reads it, and that the node's reader ignores the key and still reports the status. A
/// status whose refusal is no order's has no such key.
#[test]
fn the_order_a_refusal_answers_is_told_and_read_by_the_strict_reader_and_the_node() {
    use crate::status_v2::StatusV2;
    let (state, mut memory) = described();
    memory.refused_order_id = Some(41);
    let bytes = render(&state, &memory);
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(json["refused_order_id"], 41);
    let strict = StatusV2::parse(&bytes).expect("parses");
    assert_eq!(strict.refused_order_id, Some(41));
    let dir = kbf_host::testkit::scratch("status-refused-id");
    let path = dir.join("status.json");
    fs::write(&path, &bytes).expect("written");
    let node = kbf_update::helper::read(&path);
    assert!(matches!(node, Helper::Reported(_)), "{node:?}");

    let (state, memory) = described();
    let bytes = render(&state, &memory);
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(json.get("refused_order_id").is_none(), "{json}");
}
