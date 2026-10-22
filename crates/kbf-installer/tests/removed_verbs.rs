//! The verbs the installer no longer has, run as the binary: `migrate`, `uninstall`, `deploy` and
//! `update` are not subcommands, so clap refuses them (exit 2) before anything is read or changed.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

/// Catches a removed verb coming back, or one swallowed by another: each of `migrate` and
/// `uninstall` (bare, and with a flag an old operator's script passed) is an unrecognized
/// subcommand, exit 2, and says so on stderr; nothing goes to stdout.
#[test]
fn migrate_and_uninstall_are_unrecognized_subcommands() {
    for args in [
        &["migrate"][..],
        &["migrate", "--apply"],
        &["uninstall"],
        &["uninstall", "--apply"],
    ] {
        let out = Command::new(BIN).args(args).output().expect("runs");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("unrecognized subcommand"), "{args:?}: {err}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}

/// Catches `deploy` or `update` coming back, or still being read before clap sees the command
/// line (`deploy` once was the first word the binary looked at): each, bare and with the flags
/// an old script passed, is an unrecognized subcommand, exit 2, with nothing on stdout.
#[test]
fn deploy_and_update_are_unrecognized_subcommands() {
    let commit = "a".repeat(40);
    for args in [
        vec!["deploy"],
        vec!["deploy", "--commit", commit.as_str(), "--stage", "gamma"],
        vec![
            "deploy", "--apply", "--json", "--offer", "-", "--bundle", "-",
        ],
        vec!["update"],
        vec![
            "update",
            "--commit",
            commit.as_str(),
            "--frontend",
            "--api",
            "127.0.0.1:1",
        ],
    ] {
        let out = Command::new(BIN).args(&args).output().expect("runs");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("unrecognized subcommand"), "{args:?}: {err}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
