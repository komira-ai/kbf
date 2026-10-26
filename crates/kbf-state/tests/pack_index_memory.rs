//! The in-memory pack index against the conformance clauses P1 to P10 (the clauses
//! `tests/postgres_pack_index.rs` runs against Postgres), and what the pack index says when it
//! refuses a call.

use kbf_state::pack_index::conformance::run_all;
use kbf_state::pack_index::memory::MemoryPackBackend;
use kbf_state::{PackIndexError, StoreError};

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
