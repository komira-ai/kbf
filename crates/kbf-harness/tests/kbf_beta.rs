//! The `kbf-beta` binary, run as the workflow runs it.

#![cfg(target_os = "linux")]

use std::process::Command;

/// Catches: a binary that does not start, or one whose `--help` is not the library's (the
/// binary is a line of `main`; this is the one test that runs it).
#[test]
fn the_binary_runs_the_library_command_line() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-beta"))
        .args(["b0", "--help"])
        .output()
        .expect("kbf-beta runs");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--bin-dir",
        "--buck2",
        "--project",
        "--work",
        "--duration-secs",
        "--mutant",
    ] {
        assert!(text.contains(flag), "{flag} in {text}");
    }
    let refused = Command::new(env!("CARGO_BIN_EXE_kbf-beta"))
        .output()
        .expect("kbf-beta runs");
    assert_eq!(refused.status.code(), Some(2));
}
