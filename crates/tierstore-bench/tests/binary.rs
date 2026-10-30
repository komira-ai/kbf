//! The binary: its exit status and what it prints.

use std::process::Command;

use kbf_testkit::dir::TestDir;

/// Catches `main` that swallows a stopped verb: exit status 2 and the error on stderr.
#[test]
fn a_stub_verb_exits_2_and_says_what_is_not_built() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-bin");
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["report", "--result-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("report: not yet implemented"), "{stderr}");
}

/// Catches a binary that cannot print its help, which means a clap definition that does not
/// build.
#[test]
fn help_lists_the_verbs() {
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for verb in [
        "preflight",
        "provision",
        "run",
        "cleanup",
        "fault-proxy",
        "verify-teardown",
        "report",
    ] {
        assert!(text.contains(verb), "{verb}");
    }
}
