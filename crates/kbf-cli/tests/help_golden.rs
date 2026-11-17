//! `kbf --mode=storage --help` against its golden file, `tests/golden/kbf-storage-help.txt`,
//! byte for byte: the mode's command line is public, so a change to its flags or their help
//! shows up here as a diff to review.
//!
//! To rewrite a golden after an intended change:
//! `kbf --mode=storage --help > crates/kbf-cli/tests/golden/kbf-storage-help.txt`

use std::process::Command;

/// Catches a flag, a default, a description or the usage line of `kbf --mode=storage`
/// changing (or going missing) without the golden file, and with it the reviewer, seeing
/// it. On a mismatch the actual output is printed in full.
#[test]
fn storage_help_matches_the_golden_file() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf"))
        .args(["--mode=storage", "--help"])
        .output()
        .expect("spawn");
    assert!(out.status.success(), "exit {}", out.status);
    let actual = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let golden = include_str!("golden/kbf-storage-help.txt");
    assert!(
        actual == golden,
        "kbf --mode=storage --help differs from tests/golden/kbf-storage-help.txt; actual:\n{actual}"
    );
}
