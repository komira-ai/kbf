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

/// `kbf-beta` with `words`, and `stdin` as its input.
fn beta(words: &[&str], stdin: &str) -> std::process::Output {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_kbf-beta"))
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kbf-beta runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("input");
    child.wait_with_output().expect("kbf-beta ends")
}

/// The words of a `b0` run over `project`, working in `work`.
fn b0_words(project: &str, work: &str) -> Vec<String> {
    [
        "b0",
        "--bin-dir",
        "/nonexistent/bin",
        "--buck2",
        "/nonexistent/buck2",
        "--project",
        project,
        "--work",
        work,
        "--duration-secs",
        "1",
        "--settle-secs",
        "0",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Catches: the binary's own path through the library's `status`, which the library's tests
/// run only with their own closures: a run that fails ends 1 (not 0, not a panic) and says why
/// in its document; one whose directory cannot be made ends 2. The run fails before a farm
/// starts (its project is not there), so the test needs no farm; the sentinel it starts is
/// this binary, so `reap` is run for real too.
#[test]
fn the_binary_exits_1_for_a_run_that_fails_and_2_for_one_that_cannot_start() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-beta-binary");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let work = dir.join("work");
    let words = b0_words("/nonexistent/project", work.to_str().expect("utf-8"));
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let failed = beta(&words, "");
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    assert!(work.join("beta-result.json").is_file());
    let words = b0_words("/nonexistent/project", "/proc/no/such/work");
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    assert_eq!(beta(&words, "").status.code(), Some(2));
}

/// Catches: the sentinel's words read from the binary's own input: `done` ends it 0 and an
/// input that ends without it ends it 1 (the harness died).
#[test]
fn the_binary_runs_the_sentinel_on_its_input() {
    let words = [
        "reap",
        "--buck2",
        "/nonexistent/buck2",
        "--project",
        "/nonexistent/project",
        "--lanes",
        "0",
        "--run-id=r",
        "--kill-timeout-ms",
        "1",
    ];
    assert_eq!(beta(&words, "done\n").status.code(), Some(0));
    assert_eq!(beta(&words, "").status.code(), Some(1));
}
