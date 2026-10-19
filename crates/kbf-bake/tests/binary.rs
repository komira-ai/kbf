//! The `kbf-bake` binary: its exit codes and what it prints, through a real process.

use std::process::Command;

fn bake() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kbf-bake"))
}

/// Catches a binary that does not carry the library's exit codes: the tag body of the golden
/// verdict on standard output with exit 0, usage errors with exit 2.
#[test]
fn the_binary_exits_with_the_codes_of_the_command_line() {
    let golden = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/gamma-verdict.json"
    );
    let ok = bake()
        .args(["tag-message", "--verdict", golden])
        .output()
        .unwrap();
    assert_eq!(ok.status.code(), Some(0));
    assert_eq!(ok.stdout, std::fs::read(golden).unwrap());
    let bad = bake().output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("Usage"));
}
