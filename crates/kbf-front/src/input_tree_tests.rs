//! The decoded-directory LRU on its own: the cases the walk cannot reach (the same
//! directory inserted twice, a verification for a directory no longer kept).

use std::sync::Arc;

use kbf_types::{Digest, FarmTime};

use super::{Decoded, ENTRY_OVERHEAD, InputTreeConfig, Lru};

fn digest(n: u8) -> Digest {
    kbf_segments::sha256(&[n])
}

fn decoded(files: usize) -> Arc<Decoded> {
    Arc::new(Decoded {
        files: vec![digest(0); files].into_boxed_slice(),
        dirs: Box::default(),
    })
}

fn config(entries: usize, bytes: u64) -> InputTreeConfig {
    InputTreeConfig {
        max_entries: entries.try_into().expect("nonzero"),
        max_bytes: bytes,
        ..InputTreeConfig::DEFAULT
    }
}

/// Catches: a directory inserted twice (two flights racing across an eviction) counted
/// twice in bytes or entered twice in the recency order, so that evicting it later
/// leaves a phantom cost behind.
#[test]
fn inserting_a_directory_twice_keeps_one_entry_and_one_cost() {
    let mut lru = Lru::new(config(8, u64::MAX));
    lru.insert(digest(1), decoded(2));
    let cost = lru.bytes;
    lru.insert(digest(1), decoded(5));
    assert_eq!(
        (lru.entries.len(), lru.order.len(), lru.bytes),
        (1, 1, cost)
    );
    assert_eq!(cost, decoded(2).cost());
}

/// Catches: a verification landing on a directory the bound already evicted, which
/// would invent an entry with no directory (and no cost) behind it.
#[test]
fn a_verification_for_an_evicted_directory_is_dropped() {
    let mut lru = Lru::new(config(1, u64::MAX));
    lru.insert(digest(1), decoded(1));
    lru.insert(digest(2), decoded(1));
    lru.verified(&digest(1), FarmTime::from_millis(9));
    assert_eq!(lru.entries.len(), 1);
    assert!(lru.get(&digest(1)).is_none());
}

/// Catches: a later, shorter verification lowering an earlier longer one, and the
/// byte bound counting less than the directory's cost.
#[test]
fn a_verification_never_lowers_and_bytes_follow_cost() {
    let mut lru = Lru::new(config(8, 2 * ENTRY_OVERHEAD));
    lru.insert(digest(1), decoded(0));
    lru.verified(&digest(1), FarmTime::from_millis(9));
    lru.verified(&digest(1), FarmTime::from_millis(5));
    let (_, until) = lru.get(&digest(1)).expect("kept");
    assert_eq!(until, Some(FarmTime::from_millis(9)));
    lru.insert(digest(2), decoded(0));
    lru.insert(digest(3), decoded(0));
    assert_eq!(lru.bytes, 2 * ENTRY_OVERHEAD);
    assert_eq!(lru.entries.len(), 2);
}
