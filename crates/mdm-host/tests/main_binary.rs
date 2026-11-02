//! The program itself: its argument handling, exit codes and the random source.

use std::process::Command;

use mdm_host::system_random;

fn mdm_host(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mdm-host"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Catches a main that does not pass the exit code through: help exits 0, a command line the
/// parser refuses exits 2, and a refused verb exits 2 with REFUSED on stderr. The verb used
/// (a relative `--dir`, or root) refuses before doing anything, whoever runs the test.
#[test]
fn the_program_exits_as_documented() {
    let (code, out, _) = mdm_host(&["--help"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("Usage: mdm-host"));
    let (code, out, err) = mdm_host(&[]);
    assert_eq!((code, out.as_str()), (Some(2), ""));
    assert!(!err.is_empty());
    let (code, _, err) = mdm_host(&["fetch", "--dir", "relative/dir"]);
    assert_eq!(code, Some(2));
    assert!(err.starts_with("REFUSED: "), "{err}");
}

/// Catches a random source that returns zeros or the same bytes twice: a secret made from it
/// would be guessable.
#[test]
fn the_system_random_source_fills_the_buffer_with_fresh_bytes() {
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    system_random(&mut a).unwrap();
    system_random(&mut b).unwrap();
    assert_ne!(a, [0u8; 32]);
    assert_ne!(a, b);
}
