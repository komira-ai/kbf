//! The installed binary, run as sshd runs it: no arguments, the request in the
//! environment. Only refusals are driven here: they run nothing, on any machine the tests
//! run on, and the answer is the same line in every case.
//!
//! Catches: a binary that reads its request from anywhere but `SSH_ORIGINAL_COMMAND`
//! (arguments, standard input), one that does not exit 2 on a refusal, and one that
//! prints more than the closed line.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Output, Stdio};

const REFUSED: &str = "{\"result\":\"refused\",\"reason\":\"bad_command\"}\n";

fn run(request: Option<&OsStr>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kbf-host-install"));
    command
        .args(args)
        .env_remove("SSH_ORIGINAL_COMMAND")
        .env_remove("SSH_CONNECTION")
        .env_remove("SSH_CLIENT")
        .env_remove("SSH_TTY")
        .env_remove("KBF_HOST_INSTALL_UNIT")
        .stdin(Stdio::null());
    if let Some(request) = request {
        command.env("SSH_ORIGINAL_COMMAND", request);
    }
    command.output().unwrap()
}

/// As root the program would write its audit line to the real log: do not.
fn as_root() -> bool {
    std::fs::metadata("/proc/self").unwrap().uid() == 0
}

#[test]
fn a_request_outside_the_grammar_is_refused() {
    if as_root() {
        return;
    }
    for request in ["query x", "install kbf=1 from candidate ", "Query", ""] {
        let out = run(Some(OsStr::new(request)), &[]);
        assert_eq!(out.status.code(), Some(2), "{request:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            REFUSED,
            "{request:?}"
        );
    }
    let out = run(Some(OsStr::from_bytes(b"query\xff")), &[]);
    assert_eq!(out.status.code(), Some(2), "not UTF-8");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), REFUSED);
}

#[test]
fn only_the_environment_is_a_request() {
    if as_root() {
        return;
    }
    // No variable: an interactive login that the key's `command=` caught.
    let out = run(None, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), REFUSED);
    // Arguments are not a request, even a well-formed one.
    let out = run(None, &["query"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), REFUSED);
}

/// The body of the install unit is chosen by the one variable alone, with no ssh variable
/// around: a request that is not an install is refused there, which a query would not be.
/// (It runs nothing and writes no audit line, so it is safe to drive on any machine.)
#[test]
fn the_unit_variable_selects_the_unit_body() {
    for request in ["query", "install kbf=1;id from candidate", ""] {
        let out = Command::new(env!("CARGO_BIN_EXE_kbf-host-install"))
            .env_remove("SSH_ORIGINAL_COMMAND")
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_CLIENT")
            .env_remove("SSH_TTY")
            .env("KBF_HOST_INSTALL_UNIT", request)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{request:?}");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), REFUSED, "{request:?}");
    }
}
