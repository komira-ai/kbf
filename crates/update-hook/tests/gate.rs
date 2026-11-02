//! Kill points and the directory comparisons the harness is built on.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use update_hook::conformance::gate::{self, Gate};
use update_hook::conformance::{Entry, Sandbox, changes, snapshot};

mod support;
use support::Scratch;

fn os(s: &str) -> Option<OsString> {
    Some(OsString::from(s))
}

/// Catches a gate that stops a hook the harness did not ask to stop, or reads the target wrong.
#[test]
fn the_gate_is_read_from_a_directory_and_a_target_number() {
    assert_eq!(Gate::parse(None, os("3")), None);
    assert_eq!(Gate::parse(None, None), None);
    assert!(Gate::parse(os("/d"), None).is_some());
    // A missing or unreadable target is zero: count the points, stop at none.
    for target in [None, os("none"), os("-1"), os("")] {
        let scratch = Scratch::new("gate-no-target");
        let gate = Gate::parse(Some(scratch.path().into()), target).unwrap();
        let stops = Cell::new(0);
        for _ in 0..3 {
            gate.reach_then(&|| stops.set(stops.get() + 1));
        }
        assert_eq!(stops.get(), 0);
    }
}

/// Catches a count that does not count: the harness learns how many kill points a step has
/// from it, and a count of zero is "no kill points".
#[test]
fn each_point_is_counted_and_only_the_target_stops() {
    let scratch = Scratch::new("gate-count");
    let gate = Gate::parse(Some(scratch.path().into()), os("2")).unwrap();
    assert_eq!((gate.count(), gate.count(), gate.count()), (1, 2, 3));

    let fresh = Scratch::new("gate-target");
    let gate = Gate::parse(Some(fresh.path().into()), os("2")).unwrap();
    let stopped_at = Cell::new(Vec::new());
    for n in 1..=4 {
        gate.reach_then(&|| {
            let mut seen = stopped_at.take();
            seen.push(n);
            stopped_at.set(seen);
        });
    }
    assert_eq!(stopped_at.take(), vec![2]);
    // One byte per point, so the harness reads the count from the size.
    assert_eq!(
        fs::metadata(fresh.path().join("gate-count")).unwrap().len(),
        4
    );
}

/// Catches a gate that cannot keep its count reporting a point number that could match the
/// target.
#[test]
fn a_gate_that_cannot_count_reports_zero() {
    let scratch = Scratch::new("gate-missing-dir");
    let gate = Gate::parse(Some(scratch.path().join("absent").into()), os("1")).unwrap();
    assert_eq!(gate.count(), 0);
}

/// Catches a kill point that does anything outside the harness.
#[test]
fn outside_the_harness_a_point_does_nothing() {
    assert_eq!(Gate::from_env(), None);
    gate::point();
}

/// Catches a parked step that does not announce itself, which would leave the harness waiting
/// for ever, and one that returns, which would let the step run on.
#[test]
fn a_parked_step_announces_itself_and_does_not_return() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        gate::park_with(&|| {
            let _ = tx.send(());
        })
    });
    rx.recv().expect("the parked thread announced");
    gate::announce_parked();
    assert!(gate::PARKED.contains("parked"));
    assert_eq!(gate::GATE_DIR_VAR, "UPDATE_HOOK_GATE_DIR");
    assert_eq!(gate::KILL_AT_VAR, "UPDATE_HOOK_KILL_AT");
}

/// Catches a snapshot that does not tell a file, a directory and a link apart, or misses a
/// change the harness must see.
#[test]
fn snapshots_see_files_directories_and_links_and_what_changed() {
    let scratch = Scratch::new("snapshot");
    let root = scratch.path();
    fs::create_dir(root.join("d")).unwrap();
    fs::write(root.join("d/f"), "one").unwrap();
    fs::write(root.join("g"), "same").unwrap();
    std::os::unix::fs::symlink("v1", root.join("l")).unwrap();
    let before = snapshot(root).unwrap();
    assert_eq!(before.get(&PathBuf::from("d")), Some(&Entry::Dir));
    assert_eq!(
        before.get(&PathBuf::from("d/f")),
        Some(&Entry::File(b"one".to_vec()))
    );
    assert_eq!(
        before.get(&PathBuf::from("l")),
        Some(&Entry::Link(PathBuf::from("v1")))
    );
    assert_eq!(
        changes(&before, &snapshot(root).unwrap()),
        Vec::<String>::new()
    );

    fs::write(root.join("d/f"), "two").unwrap();
    fs::remove_file(root.join("l")).unwrap();
    fs::write(root.join("new"), "").unwrap();
    let mut moved = changes(&before, &snapshot(root).unwrap());
    moved.sort();
    assert_eq!(moved, ["added new", "changed d/f", "removed l"]);
    assert!(snapshot(&root.join("absent")).is_err());
}

/// Catches a sandbox whose directories are not where the harness and the fake look for them.
#[test]
fn a_sandbox_has_its_five_directories() {
    let scratch = Scratch::new("sandbox");
    let sandbox = Sandbox::create(scratch.path().join("run")).unwrap();
    for dir in [
        sandbox.artifact(),
        sandbox.record(),
        sandbox.world(),
        sandbox.outside(),
        sandbox.control(),
    ] {
        assert!(dir.is_dir(), "{dir:?}");
        assert!(dir.starts_with(sandbox.root()));
    }
}
