//! The memory half of the `Directory` cache: its bound, its order, and what it refuses to keep.

use kbf_types::{Digest, DigestFunction};

use super::*;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// A `Directory` of one file named `name`: it costs 256 + 64 + the name's length.
fn one(name: &str) -> KeptDirectory {
    KeptDirectory {
        files: vec![(name.to_owned(), digest(200), false)],
        ..KeptDirectory::default()
    }
}

/// Asserts that a `Directory` held already is not replaced or counted twice, and that a
/// `Directory` costing more than the bound is not kept.
#[test]
fn a_directory_held_stays_and_one_over_the_bound_is_not_kept() {
    let cache = DirectoryCache::in_memory(400);
    cache.remember(digest(1), &one("a"));
    cache.remember(digest(1), &one("b"));
    assert_eq!(cache.recent(&digest(1)), Some(one("a")));
    assert_eq!(cache.lock().used, 321);
    cache.remember(digest(2), &one(&"n".repeat(200)));
    assert_eq!(cache.recent(&digest(2)), None);
}

/// Asserts that a use makes a `Directory` the most recently used: of two kept, the one not
/// read last goes out when a third needs room.
#[test]
fn the_least_recently_used_directory_goes_out_first() {
    let cache = DirectoryCache::in_memory(700);
    cache.remember(digest(1), &one("a"));
    cache.remember(digest(2), &one("b"));
    assert!(cache.recent(&digest(1)).is_some());
    cache.remember(digest(3), &one("c"));
    assert!(cache.recent(&digest(2)).is_none());
    assert!(cache.recent(&digest(1)).is_some());
    assert!(cache.recent(&digest(3)).is_some());
}

/// Asserts that the cost counts a symlink's name and target.
#[test]
fn a_symlink_costs_its_name_and_its_target() {
    let kept = KeptDirectory {
        symlinks: vec![("ab".to_owned(), "cde".to_owned())],
        ..KeptDirectory::default()
    };
    assert_eq!(cost(&kept), 256 + 64 + 5);
}
