//! The verbs the installer no longer has, run as the binary: `migrate` and `uninstall` are not
//! subcommands, so clap refuses them (exit 2) before anything is read or changed.

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
