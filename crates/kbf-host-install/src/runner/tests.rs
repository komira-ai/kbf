use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::{Cmd, Exec, LANG, PATH, Runner, SystemRunner, fake};

fn cmd(program: &str, args: &[&str]) -> Cmd {
    Cmd {
        program: program.into(),
        args: args.iter().map(|a| (*a).into()).collect(),
        timeout: None,
    }
}

fn within(program: &str, args: &[&str], limit: Duration) -> Cmd {
    Cmd {
        timeout: Some(limit),
        ..cmd(program, args)
    }
}

/// Catches an environment that leaks: the child of the real runner sees `PATH`, `LANG=C` and `DEBIAN_FRONTEND`
/// and nothing else, whatever this process has.
#[test]
fn the_child_environment_is_cleared() {
    // This process has a full environment (cargo and the harness set several variables);
    // none of it may reach the child.
    let out = SystemRunner.run(&cmd("/usr/bin/env", &[])).unwrap();
    assert!(out.success());
    let seen: BTreeSet<String> = out.stdout_text().lines().map(str::to_owned).collect();
    let want: BTreeSet<String> = [
        format!("PATH={PATH}"),
        format!("LANG={LANG}"),
        "DEBIAN_FRONTEND=noninteractive".to_owned(),
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, want);
}

/// Catches an exit status or a stream that is lost: status, stdout and stderr all come back.
#[test]
fn status_and_streams_are_captured() {
    let failed = SystemRunner.run(&cmd("/usr/bin/false", &[])).unwrap();
    assert_eq!(failed.code, Some(1));
    assert!(!failed.success());
    let echoed = SystemRunner
        .run(&cmd("/usr/bin/env", &["/nonexistent-kbf-program"]))
        .unwrap();
    assert_eq!(echoed.code, Some(127));
    assert!(!echoed.stderr_text().is_empty());
    assert!(echoed.stdout.is_empty());
}

/// Catches a runner that swallows a program it could not start.
#[test]
fn a_missing_program_is_an_error() {
    assert!(
        SystemRunner
            .run(&cmd("/nonexistent-kbf-program", &[]))
            .is_err()
    );
}

/// A child with no stdin: `cat` reads end of file at once instead of waiting for a terminal.
#[test]
fn stdin_is_closed() {
    let out = SystemRunner.run(&cmd("/usr/bin/cat", &[])).unwrap();
    assert!(out.success() && out.stdout.is_empty());
}

#[test]
fn exec_text_replaces_invalid_bytes() {
    let exec = Exec {
        code: Some(0),
        stdout: vec![b'a', 0xff],
        stderr: vec![0xfe, b'b'],
    };
    assert_eq!(exec.stdout_text(), "a\u{fffd}");
    assert_eq!(exec.stderr_text(), "\u{fffd}b");
    assert!(exec.success());
    assert!(
        !Exec {
            code: None,
            ..exec.clone()
        }
        .success(),
        "a signal is not success"
    );
    assert!(
        !Exec {
            code: Some(3),
            ..exec
        }
        .success()
    );
}

/// The fake records what it is given and refuses a call it was not scripted for.
#[test]
fn the_fake_records_and_scripts() {
    let runner = fake::FakeRunner::new(vec![fake::ok("one")]);
    assert_eq!(runner.run(&cmd("/a", &["x"])).unwrap().stdout_text(), "one");
    assert!(runner.run(&cmd("/b", &[])).is_err());
    assert_eq!(runner.calls(), vec![cmd("/a", &["x"]), cmd("/b", &[])]);
}

/// The processes whose command line has `needle` as an argument.
fn processes_with(needle: &str) -> usize {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .filter(|line| line.split(|b| *b == 0).any(|arg| arg == needle.as_bytes()))
        .count()
}

/// Catches a limit that is not kept: a child that outlives it is killed, the call ends in
/// a timeout error at about the limit (not at the child's own end), and the child is gone
/// afterwards (killed and reaped, not left behind).
#[test]
fn a_child_past_its_limit_is_killed() {
    let marker = "31.4159265";
    let started = Instant::now();
    let error = SystemRunner
        .run(&within("/usr/bin/sleep", &[marker], Duration::from_millis(300)))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert_eq!(processes_with(marker), 0, "the sleeper was left running");
}

/// Catches a limit that breaks a child that is within it: status, both streams and a
/// stream far larger than a pipe holds (the child blocks on a full pipe unless it is read
/// while it runs) all come back.
#[test]
fn a_child_within_its_limit_is_unaffected() {
    let limit = Duration::from_secs(60);
    let ok = SystemRunner
        .run(&within("/usr/bin/true", &[], limit))
        .unwrap();
    assert!(ok.success());
    let failed = SystemRunner
        .run(&within("/usr/bin/false", &[], limit))
        .unwrap();
    assert_eq!(failed.code, Some(1));
    let stderr = SystemRunner
        .run(&within("/usr/bin/env", &["/nonexistent-kbf-program"], limit))
        .unwrap();
    assert_eq!(stderr.code, Some(127));
    assert!(!stderr.stderr_text().is_empty() && stderr.stdout.is_empty());
    let big = SystemRunner
        .run(&within(
            "/usr/bin/head",
            &["-c", "1000000", "/dev/zero"],
            limit,
        ))
        .unwrap();
    assert!(big.success());
    assert_eq!(big.stdout.len(), 1_000_000);
    let env = SystemRunner.run(&within("/usr/bin/env", &[], limit)).unwrap();
    assert!(env.stdout_text().contains("LANG=C"));
}

/// A program that cannot start is an error with a limit too, not a timeout.
#[test]
fn a_missing_program_is_an_error_within_a_limit() {
    let error = SystemRunner
        .run(&within(
            "/nonexistent-kbf-program",
            &[],
            Duration::from_secs(5),
        ))
        .unwrap_err();
    assert_ne!(error.kind(), std::io::ErrorKind::TimedOut);
}
