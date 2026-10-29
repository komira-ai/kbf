//! The in-memory pack index against the conformance clauses P1 to P10 (the clauses
//! `tests/postgres_pack_index.rs` runs against Postgres), and what the pack index says when it
//! refuses a call.

use std::sync::Arc;

use kbf_pack_index::conformance::run_all;
use kbf_pack_index::memory::MemoryPackBackend;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex as _, PackIndexError};
use kbf_state::{ManualClock, StoreError};

/// Asserts that the in-memory index passes every clause of the suite: recording and locating,
/// idempotent and conflicting records, the first location kept, batches of a thousand, touches,
/// the garbage-collection queries, concurrent records, packs that cannot exist and concurrent
/// records of one pack.
#[test]
fn the_memory_index_passes_every_clause() {
    run_all(&MemoryPackBackend);
}

/// Asserts that each refusal of the index says what it refused and what to do about it: an
/// invalid pack names the rule, a conflict names the key, a long call the count and the limit,
/// a schema without the tables the installer command, and a store failure the store's words.
#[test]
fn each_refusal_says_what_it_refused() {
    let invalid = PackIndexError::Invalid("the pack key is empty");
    let said = invalid.to_string();
    assert_eq!(said, "pack index: the pack key is empty");
    let key = "cas/d/n1/1/1".to_owned();
    let said = PackIndexError::Conflict { key }.to_string();
    assert_eq!(
        said,
        "pack index: pack \"cas/d/n1/1/1\" is already recorded with other contents"
    );
    let many = PackIndexError::TooMany {
        given: 1001,
        max: 1000,
    };
    let said = many.to_string();
    assert!(said.contains("1001 entries in one call"), "{said}");
    assert!(said.contains("at most 1000"), "{said}");
    let said = PackIndexError::NotMigrated.to_string();
    assert!(said.contains("run `kbf-installer setup`"), "{said}");
    let failed = StoreError::Failed("disk full".to_owned());
    let said = PackIndexError::from(failed).to_string();
    assert_eq!(said, "state store failure: disk full");
}

/// Asserts that the errors a repeat of the call cannot change are final, each says what it
/// refused, and the errors that may pass (a store failure, a schema migrated meanwhile) are not.
#[test]
fn a_refusal_is_final_and_a_failure_may_pass() {
    let refusals = [
        PackIndexError::Invalid("the pack key is empty"),
        PackIndexError::Conflict {
            key: "cas/d/n1/1/1".to_owned(),
        },
        PackIndexError::TooMany { given: 2, max: 1 },
        PackIndexError::Unsupported,
        PackIndexError::Refused("not allowed".to_owned()),
    ];
    for refusal in refusals {
        assert!(refusal.is_final(), "{refusal}");
    }
    let said = PackIndexError::Unsupported.to_string();
    assert_eq!(said, "pack index: the index takes no such call");
    let said = PackIndexError::Refused("not allowed".to_owned()).to_string();
    assert_eq!(said, "pack index: refused: not allowed");
    assert!(!PackIndexError::NotMigrated.is_final());
    let failed = StoreError::Failed("disk full".to_owned());
    assert!(!PackIndexError::from(failed).is_final());
}

/// A pack of the blobs `(hash byte, size, offset)`, `size` bytes long, as a daemon would record.
fn pack_of(blobs: &[(u8, u64, u64)], size: u64) -> NewPack {
    NewPack {
        key: "cas/d/n1/1/1".to_owned(),
        writer: "n1".to_owned(),
        size,
        blobs: blobs
            .iter()
            .map(|&(hash, size, offset)| PackBlob {
                digest: BlobDigest {
                    hash: [hash; 32],
                    size,
                },
                offset,
            })
            .collect(),
    }
}

/// An in-memory index on a manual clock.
fn index() -> MemoryPackIndex {
    MemoryPackIndex::new(Arc::new(ManualClock::default()))
}

/// Asserts that the index refuses a pack whose blobs share bytes: one inside another, one
/// starting a byte before the end of the one before, and the same range under two digests, each
/// with the reason and nothing recorded; and that blobs that touch end to end, in any listed
/// order, are recorded. Catches an index that points two digests at the same bytes of a pack.
#[test]
fn the_index_refuses_overlapping_blobs() {
    let overlaps = [
        pack_of(&[(1, 10, 0), (2, 3, 2)], 40),
        pack_of(&[(1, 10, 0), (2, 10, 9)], 40),
        pack_of(&[(1, 10, 5), (2, 10, 5)], 40),
        pack_of(&[(2, 10, 9), (1, 10, 0)], 40),
    ];
    for pack in &overlaps {
        let index = index();
        let refused = index.record_pack(pack).expect_err("refused");
        assert_eq!(
            refused,
            PackIndexError::Invalid("two blobs of the pack overlap")
        );
        assert!(index.pack(&pack.key).expect("asked").is_none());
    }

    let touching = pack_of(&[(2, 10, 10), (1, 10, 0), (3, 5, 20)], 40);
    let recorded = index().record_pack(&touching).expect("recorded");
    assert!(recorded.created);
}

/// Asserts that a blob of no bytes shares no bytes: the frontend's own packs hold the empty blob,
/// so a pack with one, at the start, inside another blob or at the end, is recorded. Catches an
/// overlap check that refuses the empty blob.
#[test]
fn a_blob_of_no_bytes_overlaps_nothing() {
    for at in [0, 5, 10] {
        let pack = pack_of(&[(1, 10, 0), (2, 0, at)], 40);
        let recorded = index().record_pack(&pack).expect("recorded");
        assert!(recorded.created, "at {at}");
    }
}
