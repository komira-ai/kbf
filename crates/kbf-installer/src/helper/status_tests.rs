//! `status.json` as the helper writes it: the golden bytes of the writer, and the strict reader
//! reading them. What each test proves is named on it, with the defect it catches.

use super::*;
use crate::golden::STATUS;
use crate::helper::memory::StagedFacts;
use crate::seed::Available;
use crate::status_v2::{Phase, StatusV2};
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

/// The strict reader reads what the writer writes, whole: the commits, the state word, the
/// refusal, the order acknowledged, the staged facts, the seed that is available and the source
/// the host fetches from.
///
/// Catches a writer and a reader that disagree on a key, which the golden file alone cannot see.
#[test]
fn the_strict_reader_reads_what_the_writer_writes() {
    let (state, memory) = described();
    let read = StatusV2::parse(&render(&state, &memory)).expect("parses");
    assert_eq!(read.installed_commit.as_ref().map(|c| c.as_str()), Some(A));
    assert_eq!(read.previous_commit.as_ref().map(|c| c.as_str()), Some(C));
    assert_eq!(read.staged_commit.as_ref().map(|c| c.as_str()), Some(B));
    assert_eq!(read.state, Phase::Refused);
    assert_eq!(
        read.refusal,
        "attestation: no valid attestation for kbf at commit 0123456789ab"
    );
    assert_eq!(read.acked_order_id, 41);
    let staged = read.staged.expect("staged facts");
    assert_eq!(
        (staged.protocol, staged.min_protocol, staged.state_schema),
        (12, 11, 4)
    );
    let available = read.available.expect("seed");
    assert_eq!((available.commit.as_str(), available.seq), (B, 812));
    assert_eq!(read.release_source.as_deref(), Some("bundled"));
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
/// reader reads it. A
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

    let (state, memory) = described();
    let bytes = render(&state, &memory);
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(json.get("refused_order_id").is_none(), "{json}");
}
