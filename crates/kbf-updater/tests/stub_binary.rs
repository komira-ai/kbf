//! The `kbf-updater` binary as a process: the stub answers `--version` and refuses anything
//! else until the listener lands.

use std::process::Command;

fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-updater"))
        .args(args)
        .output()
        .expect("run kbf-updater");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("stdout"),
        String::from_utf8(out.stderr).expect("stderr"),
    )
}

/// Catches a binary that prints its answer on the wrong stream or exits zero on a usage error
/// (a unit file would then start a "service" that serves nothing and call it healthy).
#[test]
fn version_goes_to_stdout_with_zero_and_usage_to_stderr_with_two() {
    let (code, out, err) = run(&["--version"]);
    assert_eq!(code, Some(0));
    assert!(out.starts_with("kbf-updater 0."), "{out}");
    assert_eq!(err, "");

    let (code, out, err) = run(&[]);
    assert_eq!(code, Some(2));
    assert_eq!(out, "");
    assert!(err.contains("usage: kbf-updater --version"), "{err}");
}
