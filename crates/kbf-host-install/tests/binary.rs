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
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            REFUSED,
            "{request:?}"
        );
    }
}

/// The release pipeline signs the Mac binary and runs `--version` on it, before and after
/// packaging; the answer is the name and the package version, whatever request the
/// environment carries, and nothing runs.
///
/// Catches: a binary that treats `--version` as a refused request (exit 2, the closed
/// refusal line), so the signed binary cannot be checked.
#[test]
fn version_prints_the_name_and_runs_nothing() {
    for request in [None, Some("query"), Some("install kbf=1 from candidate")] {
        let out = run(request.map(OsStr::new), &["--version"]);
        assert_eq!(out.status.code(), Some(0), "{request:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            concat!("kbf-host-install ", env!("CARGO_PKG_VERSION"), "\n"),
            "{request:?}"
        );
        assert!(out.stderr.is_empty(), "{request:?}");
    }
}

/// Only the one argument asks for the version: `--version` beside anything else is refused
/// with the closed line, before the audit log is touched (it is not an sshd request, and
/// nothing it could do changes the host), so it is safe to drive as root and prints nothing
/// on stderr. (`-V` is not a version flag at all; `only_the_lone_flag_asks_for_the_version`
/// covers it.)
///
/// Catches: a refused `--version` combination that opens the audit log first, which prints
/// "audit log unavailable" for a user who cannot write it, or writes a line as root.
#[test]
fn version_with_anything_else_is_refused() {
    for args in [
        &["--version", "query"][..],
        &["--version", "--json"],
        &["--version=1"],
    ] {
        for request in [None, Some("query")] {
            let out = run(request.map(OsStr::new), args);
            assert_eq!(out.status.code(), Some(2), "{args:?} {request:?}");
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                REFUSED,
                "{args:?} {request:?}"
            );
            assert!(out.stderr.is_empty(), "{args:?} {request:?}");
        }
    }
}

/// `--serve` is the local socket's mode and takes only its own flags: with none, or with a flag
/// it does not know, it exits 2 with its usage and serves nothing; anywhere but first it is no
/// flag at all and the program is the forced command, which refuses.
#[test]
fn serve_takes_its_own_flags_and_nothing_else() {
    for args in [
        &["--serve"][..],
        &["--serve", "--socket", "/run/kbf-host-install/local.sock"],
        &["--serve", "--bogus", "1"],
        &["--serve", "--socket", "relative", "--socket-user", "u"],
    ] {
        let out = run(None, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("usage: kbf-host-install --serve"), "{err}");
    }
    if as_root() {
        return;
    }
    let out = run(None, &["query", "--serve"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), REFUSED);
    let out = run(Some(OsStr::new("query")), &["--serve", "--socket"]);
    assert_eq!(out.status.code(), Some(2), "needs a value");
}

/// `approve <service> <sha256>` is the operator's act on the generic updater and takes exactly
/// those two arguments: any other command line exits 2 with its usage and records nothing.
#[test]
fn approve_takes_a_service_and_a_digest_and_nothing_else() {
    for args in [
        &["approve"][..],
        &["approve", "tierstore"],
        &["approve", "tierstore", "not-a-digest"],
        &[
            "approve",
            "tierstore",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "x",
        ],
    ] {
        let out = run(None, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains("usage: approve <service>"), "{err}");
    }
}

/// `--migrate-to-package` is the held step that moves a hand-placed Linux host to the package
/// and takes only its own flags: with none, or with a flag it does not know, it exits 2 with its
/// usage and touches nothing; a unit that is not a daemon mode's is refused before the host is
/// looked at. (A Mac has no such step and says so.)
#[test]
fn migrate_takes_its_own_flags_and_nothing_else() {
    for args in [
        &["--migrate-to-package"][..],
        &["--migrate-to-package", "--bogus", "1"],
        &["--migrate-to-package", "--layout", "tar"],
    ] {
        let out = run(None, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        if cfg!(target_os = "linux") {
            assert!(
                err.contains("usage: kbf-host-install --migrate-to-package"),
                "{err}"
            );
        } else {
            assert!(err.contains("is for a Linux host"), "{err}");
        }
    }
    let out = run(
        None,
        &[
            "--migrate-to-package",
            "--layout",
            "deb",
            "--channel",
            "stable",
            "--unit",
            "kbf-frontend.service",
            "--dir",
            "/srv/release",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}
