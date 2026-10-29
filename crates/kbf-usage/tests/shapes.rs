//! `ShapeIndex`: action to profile, and the `KeyMap` over it.

mod support;

use std::sync::Arc;

use kbf_types::{ActionKey, Digest, DigestFunction, WorkKey, work_key};
use kbf_usage::{MAX_SHAPES, ShapeIndex};
use support::{action, digest, profile};

fn numbered(n: u64) -> ActionKey {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, hash, n),
    }
}

// Catches a get that finds nothing, or another action's profile.
#[test]
fn an_inserted_action_is_found_and_another_is_not() {
    let index = ShapeIndex::new();
    assert!(index.is_empty());
    index.insert(action("main", 1), profile("p1"));
    index.insert(action("main", 2), profile("p2"));
    assert_eq!(index.get(&action("main", 1)), Some(profile("p1")));
    assert_eq!(index.get(&action("main", 2)), Some(profile("p2")));
    assert_eq!(index.get(&action("main", 3)), None);
    assert_eq!(index.len(), 2);
    assert!(!index.is_empty());
}

// Catches an index that ignores the instance: the profile is named from the instance too,
// so the same digest on another instance is another action.
#[test]
fn the_same_digest_on_another_instance_is_another_action() {
    let index = ShapeIndex::new();
    index.insert(action("main", 1), profile("p1"));
    assert_eq!(index.get(&action("other", 1)), None);
}

// Catches an insert that adds a second entry for one action instead of replacing the
// profile, and one that does not make the action the newest.
#[test]
fn inserting_an_action_again_replaces_its_profile() {
    let index = ShapeIndex::new();
    index.insert(action("main", 1), profile("old"));
    index.insert(action("main", 1), profile("new"));
    assert_eq!(index.len(), 1);
    assert_eq!(index.get(&action("main", 1)), Some(profile("new")));
    assert_eq!(index.evicted(), 0);
}

// Catches an index that grows without bound, one that evicts the newest, and one that
// forgets everything: a full index forgets exactly the action put in longest ago.
#[test]
fn a_full_index_forgets_the_action_put_in_longest_ago() {
    let index = ShapeIndex::with_capacity(3);
    for tag in 1..=3 {
        index.insert(action("main", tag), profile(&format!("p{tag}")));
    }
    assert_eq!(index.evicted(), 0);
    index.insert(action("main", 4), profile("p4"));
    assert_eq!(index.len(), 3);
    assert_eq!(index.evicted(), 1);
    assert_eq!(index.get(&action("main", 1)), None);
    for tag in 2..=4 {
        assert_eq!(
            index.get(&action("main", tag)),
            Some(profile(&format!("p{tag}")))
        );
    }
    index.insert(action("main", 5), profile("p5"));
    assert_eq!(index.get(&action("main", 2)), None);
    assert_eq!(index.evicted(), 2);
}

// Catches a replace that keeps the action's old place in the order: after action 1 is put
// in again, action 2 is the oldest.
#[test]
fn an_action_put_in_again_is_the_newest() {
    let index = ShapeIndex::with_capacity(2);
    index.insert(action("main", 1), profile("p1"));
    index.insert(action("main", 2), profile("p2"));
    index.insert(action("main", 1), profile("p1"));
    index.insert(action("main", 3), profile("p3"));
    assert_eq!(index.get(&action("main", 2)), None);
    assert_eq!(index.get(&action("main", 1)), Some(profile("p1")));
    assert_eq!(index.evicted(), 1);
}

// Catches a remove that leaves the entry or its place in the order: the freed slot must
// be reusable without evicting, and a removed action must not be evicted later in place
// of a live one.
#[test]
fn a_removed_action_frees_its_slot() {
    let index = ShapeIndex::with_capacity(2);
    index.insert(action("main", 1), profile("p1"));
    index.insert(action("main", 2), profile("p2"));
    assert!(index.remove(&action("main", 1)));
    assert!(!index.remove(&action("main", 1)));
    assert_eq!(index.get(&action("main", 1)), None);
    index.insert(action("main", 3), profile("p3"));
    assert_eq!(index.evicted(), 0);
    assert_eq!(index.len(), 2);
    index.insert(action("main", 4), profile("p4"));
    assert_eq!(index.get(&action("main", 2)), None);
    assert_eq!(index.get(&action("main", 3)), Some(profile("p3")));
}

// Catches a capacity of zero that would refuse every insert or evict on an empty index.
#[test]
fn a_capacity_of_zero_holds_one_action() {
    let index = ShapeIndex::with_capacity(0);
    index.insert(action("main", 1), profile("p1"));
    index.insert(action("main", 2), profile("p2"));
    assert_eq!(index.len(), 1);
    assert_eq!(index.get(&action("main", 2)), Some(profile("p2")));
    assert_eq!(index.evicted(), 1);
}

// Catches a default bound that is not the stated one: the index holds MAX_SHAPES actions
// and the next one pushes out the first.
#[test]
fn the_default_bound_is_max_shapes() {
    assert_eq!(MAX_SHAPES, 65_536);
    let index = ShapeIndex::default();
    for n in 0..=MAX_SHAPES as u64 {
        index.insert(numbered(n), profile("p"));
    }
    assert_eq!(index.len(), MAX_SHAPES);
    assert_eq!(index.evicted(), 1);
    assert_eq!(index.get(&numbered(0)), None);
    assert_eq!(index.get(&numbered(1)), Some(profile("p")));
}

// Catches a key map that reads another index, a snapshot taken when it was made, a lookup
// by dedup text alone (the instance is the class), and one that answers for a work key
// that is not an action's.
#[test]
fn the_key_map_reads_the_index_by_work_key() {
    let index = Arc::new(ShapeIndex::new());
    let map = index.key_map();
    assert_eq!(map(&work_key("main", digest(1))), None);
    index.insert(action("main", 1), profile("p1"));
    assert_eq!(map(&work_key("main", digest(1))), Some(profile("p1")));
    assert_eq!(map(&work_key("main", digest(2))), None, "unknown digest");
    assert_eq!(map(&work_key("other", digest(1))), None, "other instance");
    let not_a_digest = WorkKey::new("main", "not-a-digest");
    assert_eq!(map(&not_a_digest), None);
    assert_eq!(index.lookup(&not_a_digest), None);
    assert!(index.remove(&action("main", 1)));
    assert_eq!(map(&work_key("main", digest(1))), None);
}
