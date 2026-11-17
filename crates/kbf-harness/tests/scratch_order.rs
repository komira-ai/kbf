//! The farm's scratch directory is removed after its processes are gone. The test runs
//! against a stand-in process of this package (`kbf-harness-watcher`), which only the
//! package's own integration tests can name with `CARGO_BIN_EXE_*`; having one makes
//! `cargo test` build it.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use kbf_harness::farm::{Farm, Role};
use kbf_harness::procs::Spec;
use kbf_testkit::dir::TestDir;

/// Catches: the scratch removed while a process still runs in it (a field order, or a Drop,
/// that takes the directory first). The watcher spins until the marker it was started beside
/// is gone, then records that; the directory holds many files, so its removal takes long
/// enough for a watcher still alive to see it. A farm that kills first leaves no watcher to
/// write anything.
#[test]
fn a_farm_reaps_its_processes_before_it_removes_the_scratch() {
    let outside = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "harness-scratch-order");
    let scratch = TestDir::under(outside.path(), "own");
    let path = scratch.path().to_path_buf();
    let marker = path.join("marker");
    std::fs::write(&marker, b"x").expect("write the marker");
    for n in 0..3000 {
        std::fs::write(path.join(format!("f{n}")), b"x").expect("write a filler");
    }
    let (up, saw_gone) = (outside.path().join("up"), outside.path().join("saw_gone"));
    let watcher = Spec {
        name: Role::Node(1).to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_kbf-harness-watcher")),
        argv: [&up, &marker, &saw_gone]
            .map(|p| p.display().to_string())
            .to_vec(),
        env: Vec::new(),
        cwd: path.clone(),
    };
    let farm = Farm::supervise(&path, vec![(Role::Node(1), watcher)]).owning(scratch);
    let until = std::time::Instant::now() + Duration::from_secs(30);
    while !up.exists() {
        assert!(
            std::time::Instant::now() < until,
            "the watcher never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(farm);
    assert!(!path.exists(), "{} is left behind", path.display());
    assert!(
        !saw_gone.exists(),
        "the scratch was removed while a process still ran in it"
    );
}
