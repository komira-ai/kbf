//! The phase timings a task writing into a put directory adds to `timing`: `walk_ms`,
//! `hash_ms`, `write_ms` and `commit_ms`, each optional, in every version.

use kbf_task_pack::{ManifestError, TaskManifest, Timing};

use crate::task_manifest::{base, fixture, parse_fixture, parse_v2};

/// The timing of the fixtures with phases: today's three and the phases that are set.
fn with_phases(walk: u64, hash: Option<u64>, write: Option<u64>, commit: u64) -> Timing {
    Timing {
        walk_ms: Some(walk),
        hash_ms: hash,
        write_ms: write,
        commit_ms: Some(commit),
        ..base().timing
    }
}

/// Asserts the new data: a version 1 manifest with all four phases, one with two of them, and
/// the version 2 manifest of a tree job with all four each parse to their values, a phase of 0
/// included, and write back byte for byte, the phases after `pack_ms` in their order and an
/// absent one left out.
#[test]
fn a_manifest_with_phase_timings_parses_and_writes_back_byte_for_byte() {
    let all = TaskManifest {
        timing: with_phases(3, Some(1), Some(0), 5),
        ..base()
    };
    let some = TaskManifest {
        timing: with_phases(3, None, None, 5),
        ..base()
    };
    for (name, expected) in [
        ("valid/timing_phases.json", all),
        ("valid/timing_some_phases.json", some),
    ] {
        assert_eq!(parse_fixture(name).expect(name), expected, "{name}");
        assert_eq!(expected.to_json().expect(name), fixture(name), "{name}");
    }

    let name = "valid_v2/timing_phases_tree.json";
    let tree = parse_v2(name).expect(name);
    let without = parse_v2("valid_v2/inputs_from_tree.json").expect("without");
    assert_eq!(tree.timing, with_phases(3, Some(1), Some(0), 5));
    assert_eq!(
        TaskManifest {
            timing: without.timing,
            ..tree.clone()
        },
        without
    );
    assert_eq!(tree.version(), 2);
    assert_eq!(tree.to_json_with(0).expect(name), fixture(name));
}

/// Asserts the old data: a manifest without phases (what every writer before them wrote) reads
/// with each phase `None` and writes back without them, so a run that measures none writes the
/// bytes it wrote before.
#[test]
fn a_manifest_without_phases_reads_as_none_and_writes_none() {
    let read = parse_fixture("valid/minimal.json").expect("minimal");
    let timing = read.timing;
    assert_eq!(
        [
            timing.walk_ms,
            timing.hash_ms,
            timing.write_ms,
            timing.commit_ms
        ],
        [None; 4]
    );
    let json = read.to_json().expect("json");
    assert_eq!(json, fixture("valid/minimal.json"));
    assert!(!String::from_utf8(json).expect("utf-8").contains("walk_ms"));
}

/// Asserts that a phase is held to `i64` like every other timing: beyond it is refused with the
/// phase's field name, read or written, and `i64::MAX` itself is allowed.
#[test]
fn a_phase_beyond_i64_is_refused() {
    let over = i64::MAX.unsigned_abs() + 1;
    assert!(matches!(
        parse_fixture("invalid/timing_phase_beyond_i64.json"),
        Err(ManifestError::Advisory {
            field: "timing.walk_ms",
            value
        }) if value == over
    ));
    type Setter = fn(&mut Timing, Option<u64>);
    let phases: [(&str, Setter); 4] = [
        ("timing.walk_ms", |t, v| t.walk_ms = v),
        ("timing.hash_ms", |t, v| t.hash_ms = v),
        ("timing.write_ms", |t, v| t.write_ms = v),
        ("timing.commit_ms", |t, v| t.commit_ms = v),
    ];
    for (name, set) in phases {
        let mut manifest = base();
        set(&mut manifest.timing, Some(i64::MAX.unsigned_abs()));
        assert!(manifest.to_json().is_ok(), "{name} at i64::MAX");
        set(&mut manifest.timing, Some(over));
        assert!(
            matches!(manifest.to_json(), Err(ManifestError::Advisory { field, value }) if field == name && value == over),
            "{name}"
        );
    }
}
