//! The `kbf-mac-session` binary off macOS: its flags parse, and it refuses to serve.

#![cfg(not(target_os = "macos"))]

use std::process::Command;

fn helper() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kbf-mac-session"))
}

/// Catches: a binary that would bind a socket on a platform whose host it lacks.
#[test]
fn it_refuses_to_serve_off_macos() {
    let output = helper()
        .args(["--daemon-requirement", "cdhash H\"00\""])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("serves only on macOS"), "{stderr}");
}

#[test]
fn the_requirement_flag_is_required() {
    let output = helper().output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--daemon-requirement"), "{stderr}");
}

/// Catches: the `node-admin` mode reaching for a Mac's tools on a platform that has
/// none, or `node-admin` parsed as one of the helper's flags.
#[test]
fn node_admin_refuses_off_macos() {
    let output = helper()
        .args(["node-admin", "phase0", "status"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr,
        "REFUSED: kbf-mac-session node-admin runs on a Mac\n"
    );
}
