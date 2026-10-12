//! `act_test_hook!`: this test binary, run through links `act::link_self` makes,
//! answers as `kbf-act` or as a fake, and as itself under any other name.

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_testkit::act::{NAME, link_self};

kbf_testkit::act_test_hook!();

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Catches: the hook not run at start-up (the link would start the test harness, which
/// takes the steps for test names and runs none); the step's exit status lost; a link
/// named for a fake not answering as that fake; and the hook answering under every
/// name (the test binary could no longer run its own tests: here, list them).
#[test]
fn a_test_binary_answers_through_its_links() {
    let dir = dir("hook-links");
    let act = dir.join(NAME);
    link_self(&act).expect("link");
    let out = Command::new(&act)
        .args(["echo", "hooked", ";", "exit", "7"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    assert_eq!(text(&out.stdout), "hooked\n");

    let xcrun = dir.join("xcrun");
    link_self(&xcrun).expect("link");
    std::fs::write(dir.join("xcrun.rules"), "--find *\t-\techo /fake/$2\n").expect("rules");
    let out = Command::new(&xcrun)
        .args(["--find", "clang"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "/fake/clang\n");

    let other = dir.join("hook-tests");
    link_self(&other).expect("link");
    let out = Command::new(&other)
        .args(["--list", "a_test_binary_answers_through_its_links"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        text(&out.stdout).contains("a_test_binary_answers_through_its_links: test"),
        "{out:?}"
    );
}
