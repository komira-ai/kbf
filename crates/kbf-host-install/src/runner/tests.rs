use std::collections::BTreeSet;

use super::{fake, Cmd, Exec, Runner, SystemRunner, LANG, PATH};

fn cmd(program: &str, args: &[&str]) -> Cmd {
    Cmd { program: program.into(), args: args.iter().map(|a| (*a).into()).collect() }
}

/// Catches an environment that leaks: the child of the real runner sees `PATH` and `LANG=C`
/// and nothing else, whatever this process has.
#[test]
fn the_child_environment_is_cleared() {
    // This process has a full environment (cargo and the harness set several variables);
    // none of it may reach the child.
    let out = SystemRunner.run(&cmd("/usr/bin/env", &[])).unwrap();
    assert!(out.success());
    let seen: BTreeSet<String> = out.stdout_text().lines().map(str::to_owned).collect();
    let want: BTreeSet<String> =
        [format!("PATH={PATH}"), format!("LANG={LANG}")].into_iter().collect();
    assert_eq!(seen, want);
}

/// Catches an exit status or a stream that is lost: status, stdout and stderr all come back.
#[test]
fn status_and_streams_are_captured() {
    let failed = SystemRunner.run(&cmd("/usr/bin/false", &[])).unwrap();
    assert_eq!(failed.code, Some(1));
    assert!(!failed.success());
    let echoed = SystemRunner.run(&cmd("/usr/bin/env", &["/nonexistent-kbf-program"])).unwrap();
    assert_eq!(echoed.code, Some(127));
    assert!(!echoed.stderr_text().is_empty());
    assert!(echoed.stdout.is_empty());
}

/// Catches a runner that swallows a program it could not start.
#[test]
fn a_missing_program_is_an_error() {
    assert!(SystemRunner.run(&cmd("/nonexistent-kbf-program", &[])).is_err());
}

/// A child with no stdin: `cat` reads end of file at once instead of waiting for a terminal.
#[test]
fn stdin_is_closed() {
    let out = SystemRunner.run(&cmd("/usr/bin/cat", &[])).unwrap();
    assert!(out.success() && out.stdout.is_empty());
}

#[test]
fn exec_text_replaces_invalid_bytes() {
    let exec = Exec { code: Some(0), stdout: vec![b'a', 0xff], stderr: vec![0xfe, b'b'] };
    assert_eq!(exec.stdout_text(), "a\u{fffd}");
    assert_eq!(exec.stderr_text(), "\u{fffd}b");
    assert!(exec.success());
    assert!(!Exec { code: None, ..exec.clone() }.success(), "a signal is not success");
    assert!(!Exec { code: Some(3), ..exec }.success());
}

/// The fake records what it is given and refuses a call it was not scripted for.
#[test]
fn the_fake_records_and_scripts() {
    let runner = fake::FakeRunner::new(vec![fake::ok("one")]);
    assert_eq!(runner.run(&cmd("/a", &["x"])).unwrap().stdout_text(), "one");
    assert!(runner.run(&cmd("/b", &[])).is_err());
    assert_eq!(runner.calls(), vec![cmd("/a", &["x"]), cmd("/b", &[])]);
}
