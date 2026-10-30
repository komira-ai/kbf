//! The binary: its exit status and what it prints.

use std::process::Command;

use kbf_testkit::dir::TestDir;

/// Catches a digest line that is not the SHA-256 of the file that runs (the CI log line the farm
/// run is checked against): the line is exactly `tierstore-bench sha256 <hex of the binary>`.
#[test]
fn self_digest_prints_the_sha256_of_the_executable_on_one_line() {
    use sha2::{Digest as _, Sha256};
    let exe = env!("CARGO_BIN_EXE_tierstore-bench");
    let out = Command::new(exe).arg("self-digest").output().unwrap();
    assert!(out.status.success());
    let want = format!(
        "tierstore-bench sha256 {}\n",
        hex::encode(Sha256::digest(std::fs::read(exe).unwrap()))
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

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
        "self-digest",
        "fault-proxy",
        "verify-teardown",
        "report",
    ] {
        assert!(text.contains(verb), "{verb}");
    }
}
