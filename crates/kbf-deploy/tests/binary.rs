//! The `kbf-deploy` binary. (The library's `cli` tests run the same `run` in-process,
//! against a fake host.)

use std::process::Command;

/// Catches a binary that does not reach the command line parser, or one that takes an
/// unknown subcommand as a plan with nothing to do (exit 0) instead of bad flags.
#[test]
fn the_binary_refuses_an_unknown_subcommand() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-deploy"))
        .arg("frobnicate")
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("frobnicate"), "{stderr}");
}
