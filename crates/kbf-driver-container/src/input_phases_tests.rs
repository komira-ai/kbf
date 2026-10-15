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
    };
    assert_eq!(
        phases.to_string(),
        "action_ms=1 image_ms=2 walk_ms=3 dir_reads=4 dirs_made=5 trees_ms=6 \
         trees_built=7 link_ms=8 links=9 copies=10 prep_ms=11 chown_ms=12 cgroup_ms=13"
    );
}

/// `millis` is the elapsed time in whole milliseconds, not seconds or microseconds.
#[test]
fn millis_counts_whole_milliseconds() {
    let started = Instant::now() - Duration::from_millis(1500);
    let ms = millis(started);
    assert!((1500..60_000).contains(&ms), "{ms}");
}
