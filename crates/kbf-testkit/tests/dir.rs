//! `TestDir`: removed on drop, kept only for a panicking test with `KBF_KEEP_TMP` set.
//!
//! The keep switch is an environment variable and setting one in this process would be
//! `unsafe`, so the panicking cases run in a child: this test binary again, running the
//! `#[ignore]`d `child_*` test named by `KBF_DIR_CASE`, with the variable set or not.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::dir::{KEEP_ENV, TestDir};

const ROOT: &str = env!("CARGO_TARGET_TMPDIR");

/// Catches a `TestDir` that is not removed when a passing test drops it, or whose
/// removal leaves part of the tree: files and nested directories made in it are gone
/// with it, and the parent holds nothing else of it.
#[test]
fn a_dropped_dir_and_everything_in_it_is_gone() {
    let holder = TestDir::under(ROOT, "dir-holder");
    let path = {
        let dir = TestDir::under(holder.path(), "case");
        std::fs::create_dir_all(dir.join("a/b")).expect("nested");
        std::fs::write(dir.join("a/b/file"), b"x").expect("file");
        std::fs::write(dir.join("top"), b"y").expect("file");
        assert!(dir.is_dir());
        dir.path().to_path_buf()
    };
    assert!(!path.exists(), "{} survived its drop", path.display());
    let left: Vec<_> = std::fs::read_dir(holder.path()).expect("list").collect();
    assert!(left.is_empty(), "{left:?}");
}

/// Catches two dirs of one name sharing a path (a second test would then remove or
/// write into the first's files), and a name that does not appear in the path.
#[test]
fn dirs_of_one_name_are_distinct_and_named() {
    let holder = TestDir::under(ROOT, "dir-names");
    let a = TestDir::under(holder.path(), "same");
    let b = TestDir::under(holder.path(), "same");
    assert_ne!(a.path(), b.path());
    for dir in [&a, &b] {
        let name = dir
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(
            name.starts_with(&format!("same-{}-", std::process::id())),
            "{name}"
        );
    }
}

/// Runs the child case `case` of this binary with scratch root `root`, with the keep
/// switch set or not; returns the child's stderr.
fn run_child(case: &str, root: &Path, keep: bool) -> String {
    let mut cmd = Command::new(std::env::current_exe().expect("exe"));
    cmd.args([
        "--exact",
        case,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env("KBF_DIR_ROOT", root)
    .env_remove(KEEP_ENV);
    if keep {
        cmd.env(KEEP_ENV, "1");
    }
    let out = cmd.output().expect("run child");
    assert!(!out.status.success(), "the child case panics on purpose");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The directories under `root` (one level).
fn entries(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .expect("list")
        .map(|e| e.expect("entry").path())
        .collect()
}

/// Catches `KBF_KEEP_TMP` not keeping a panicking test's dir (the files a person needs
/// to read), and the kept path not being printed.
#[test]
fn a_panicking_test_keeps_its_dir_with_the_switch() {
    let holder = TestDir::under(ROOT, "dir-keep");
    let stderr = run_child("child_panics_with_a_dir", holder.path(), true);
    let kept = entries(holder.path());
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].join("evidence").is_file());
    assert!(stderr.contains(&kept[0].display().to_string()), "{stderr}");
}

/// Catches a panicking test's dir kept without the switch (a failing run would then
/// leak as before).
#[test]
fn a_panicking_test_removes_its_dir_without_the_switch() {
    let holder = TestDir::under(ROOT, "dir-nokeep");
    run_child("child_panics_with_a_dir", holder.path(), false);
    assert!(entries(holder.path()).is_empty());
}

/// Catches the switch keeping the dir of a test that passed.
#[test]
fn a_passing_test_removes_its_dir_even_with_the_switch() {
    let holder = TestDir::under(ROOT, "dir-pass");
    let out = Command::new(std::env::current_exe().expect("exe"))
        .args(["--exact", "child_passes_with_a_dir", "--ignored"])
        .env("KBF_DIR_ROOT", holder.path())
        .env(KEEP_ENV, "1")
        .output()
        .expect("run child");
    assert!(out.status.success());
    assert!(entries(holder.path()).is_empty());
}

#[test]
#[ignore = "run by the cases above, in a child process"]
fn child_panics_with_a_dir() {
    let root = std::env::var("KBF_DIR_ROOT").expect("root");
    let dir = TestDir::under(root, "child");
    std::fs::write(dir.join("evidence"), b"e").expect("file");
    panic!("on purpose");
}

#[test]
#[ignore = "run by the cases above, in a child process"]
fn child_passes_with_a_dir() {
    let root = std::env::var("KBF_DIR_ROOT").expect("root");
    let dir = TestDir::under(root, "child");
    std::fs::write(dir.join("evidence"), b"e").expect("file");
}
