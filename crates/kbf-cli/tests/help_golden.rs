//! `kbf all-in-one --help` and `kbf storage-and-daemon --help` against their golden files,
//! `tests/golden/kbf-all-in-one-help.txt` and `tests/golden/kbf-storage-and-daemon-help.txt`,
//! byte for byte: the mode's command line
//! is public (it is `kbf storage`'s flags and `kbf-daemon`'s), so a change to its flags or
//! their help shows up here as a diff to review.
//!
//! To rewrite a golden after an intended change:
//! `kbf all-in-one --help > crates/kbf-cli/tests/golden/kbf-all-in-one-help.txt`, and
//! `kbf storage-and-daemon --help > crates/kbf-cli/tests/golden/kbf-storage-and-daemon-help.txt`

use std::process::Command;

/// Catches a flag, a default, a description or the usage line of `kbf storage-and-daemon`
/// changing (or going missing) without the golden file, and with it the reviewer, seeing
/// it. On a mismatch the actual output is printed in full.
#[test]
fn storage_and_daemon_help_matches_the_golden_file() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf"))
        .args(["storage-and-daemon", "--help"])
        .output()
        .expect("spawn");
    assert!(out.status.success(), "exit {}", out.status);
    let actual = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let golden = include_str!("golden/kbf-storage-and-daemon-help.txt");
    assert!(
        actual == golden,
        "kbf storage-and-daemon --help differs from tests/golden/kbf-storage-and-daemon-help.txt; actual:\n{actual}"
    );
}

/// Catches a flag, a default, a description or the usage line of `kbf all-in-one`
/// changing (or going missing) without the golden file, and with it the reviewer, seeing
/// it. On a mismatch the actual output is printed in full.
#[test]
fn all_in_one_help_matches_the_golden_file() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf"))
        .args(["all-in-one", "--help"])
        .output()
        .expect("spawn");
    assert!(out.status.success(), "exit {}", out.status);
    let actual = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let golden = include_str!("golden/kbf-all-in-one-help.txt");
    assert!(
        actual == golden,
        "kbf all-in-one --help differs from tests/golden/kbf-all-in-one-help.txt; actual:\n{actual}"
    );
}
