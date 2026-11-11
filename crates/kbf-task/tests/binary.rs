//! The `kbf-task` binary as the release checks it (`cargo xtask build-commit check`).

use std::process::Command;

/// Catches: `--version` not naming the commit the binary was built from, which is what the
/// release checks the binary, and the image carrying it, against: the line is `kbf-task
/// <package version>+<12 hex digits of the commit>` (or `unknown` for a source archive).
#[test]
fn the_version_names_the_commit_it_was_built_from() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .arg("--version")
        .output()
        .expect("kbf-task runs");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf-8");
    let line = text.trim_end();
    let rest = line
        .strip_prefix("kbf-task ")
        .unwrap_or_else(|| panic!("not `kbf-task <version>`: {line:?}"));
    let (package, stamp) = rest
        .split_once('+')
        .unwrap_or_else(|| panic!("no `+<commit>` after the package version: {line:?}"));
    assert_eq!(package, env!("CARGO_PKG_VERSION"), "{line:?}");
    let hex = stamp.len() == 12
        && stamp
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    assert!(hex || stamp == "unknown", "the stamp is {stamp:?}");
}

/// Catches: the binary's `main` not handing the parsed flags to the task and returning the
/// task's exit code: a token file that does not exist is a setup failure, exit code 14, before
/// anything is fetched or written.
#[test]
fn a_missing_credential_file_is_a_setup_failure_exit_code() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .args([
            "--pack-url-file",
            "/nonexistent-url",
            "--grant-url",
            "https://grant.example",
            "--grant-token-file",
            "/nonexistent-token",
            "--lease",
            "lease-1",
            "--action",
            &format!("{}/0", "0".repeat(64)),
            "--work-dir",
            "/nonexistent-work",
        ])
        .output()
        .expect("kbf-task runs");
    assert_eq!(out.status.code(), Some(14));
    let text = String::from_utf8(out.stderr).expect("utf-8");
    assert!(text.starts_with("kbf-task: "), "{text}");
}

/// Catches: `main` not handing its two hidden modes to the relay, or a mode reporting nothing:
/// `--sandbox-init` starts its Command and writes the start report on its standard input, the
/// relay with no PID namespace pending executes the Command itself, and a malformed mode exits 2.
#[cfg(target_os = "linux")]
#[test]
fn the_binary_runs_the_relay_and_the_first_process() {
    use std::io::Read as _;
    let run = |args: &[&str]| {
        let (read, write) = rustix::pipe::pipe().expect("pipe");
        let status = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
            .args(args)
            .stdin(std::process::Stdio::from(write))
            .status()
            .expect("kbf-task runs");
        let mut said = Vec::new();
        std::fs::File::from(read)
            .read_to_end(&mut said)
            .expect("the report");
        (status.code(), said)
    };
    assert_eq!(
        run(&["--sandbox-init", "--", "/usr/bin/false"]),
        (Some(1), vec![0, 0])
    );
    assert_eq!(
        run(&["--sandbox-relay", "--group", "1", "--", "/usr/bin/true"]),
        (Some(0), Vec::new())
    );
    assert_eq!(run(&["--sandbox-init"]), (Some(2), Vec::new()));
}
