//! The `inputs phases:` line's format, which measurement scripts parse.

use std::time::{Duration, Instant};

use super::*;

/// Pins the line field by field: a renamed, dropped, reordered or swapped field (each
/// value is distinct) fails here before a script parsing the logs misreads it.
#[test]
fn the_line_names_every_field_in_order() {
    let phases = InputPhases {
        action_ms: 1,
        image_ms: 2,
        write: WritePhases {
            walk_ms: 3,
            dir_reads: 4,
            dirs_made: 5,
            trees_ms: 6,
            link_ms: 8,
            links: 9,
            copies: 10,
        },
        trees_built: 7,
        prep_ms: 11,
        chown_ms: 12,
        cgroup_ms: 13,
        numa_node: None,
    };
    assert_eq!(
        phases.to_string(),
        "action_ms=1 image_ms=2 walk_ms=3 dir_reads=4 dirs_made=5 trees_ms=6 \
         trees_built=7 link_ms=8 links=9 copies=10 prep_ms=11 chown_ms=12 cgroup_ms=13"
    );
}

/// Catches `numa_node` printed when leases are not placed (the line must be what it was),
/// or not appended after the existing keys (a script reading `cgroup_ms=` last breaks).
#[test]
fn numa_node_is_appended_only_when_the_lease_is_placed() {
    let unplaced = InputPhases::default().to_string();
    assert!(unplaced.ends_with("cgroup_ms=0"), "{unplaced}");
    let placed = InputPhases {
        numa_node: Some(Spot::Node(1)),
        ..InputPhases::default()
    };
    assert_eq!(placed.to_string(), format!("{unplaced} numa_node=1"));
}

/// Catches an unpinned lease under numa-node placement printed like a lease on no node or
/// on node 0: it says `numa_node=all`, in the same place.
#[test]
fn numa_node_all_is_printed_for_an_unpinned_lease() {
    let unplaced = InputPhases::default().to_string();
    let all = InputPhases {
        numa_node: Some(Spot::All),
        ..InputPhases::default()
    };
    assert_eq!(all.to_string(), format!("{unplaced} numa_node=all"));
}

/// `millis` is the elapsed time in whole milliseconds, not seconds or microseconds.
#[test]
fn millis_counts_whole_milliseconds() {
    let started = Instant::now() - Duration::from_millis(1500);
    let ms = millis(started);
    assert!((1500..60_000).contains(&ms), "{ms}");
}
