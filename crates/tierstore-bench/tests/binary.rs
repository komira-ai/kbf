//! The binary: its exit status and what it prints.

use std::process::Command;

use kbf_testkit::dir::TestDir;

/// Catches `main` that swallows a stopped verb: exit status 2 and the error on stderr.
#[test]
fn a_stub_verb_exits_2_and_says_what_is_not_built() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-bin");
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["preflight", "--result-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("preflight: not yet implemented"),
        "{stderr}"
    );
}

/// Catches a report that is built and not printed: the table of an empty result directory is on
/// standard output and the status is 0, and a directory that is not there is a stopped verb.
#[test]
fn report_prints_its_table_and_a_missing_directory_stops_it() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-report");
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["report", "--result-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("# tierstore-bench report"));
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .args(["report", "--result-dir"])
        .arg(dir.path().join("absent"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// Catches a binary that cannot print its help, which means a clap definition that does not
/// build.
#[test]
fn help_lists_the_six_verbs() {
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
        "fault-proxy",
        "verify-teardown",
        "report",
    ] {
        assert!(text.contains(verb), "{verb}");
    }
}
