//! `kbf boot-check --mode=<mode>` end to end, as a unit's `ExecStartPre=` runs it: the real
//! binary, with no server. What it does on a mode that keeps no state in the database, on an
//! `installer.toml` that names none, and on every `installer.toml` or database that stops the
//! unit's start (exit 1, the reason on standard error, never the URL), and how it refuses a
//! command line (exit 2). The check against a real server is `boot_check_postgres.rs`.

mod cluster_support;

use cluster_support::kbf;
use kbf_cli::boot_check::{NO_DATABASE, NO_STATE};
use kbf_testkit::dir::TestDir;

/// `kbf boot-check <args>`: the exit status, standard output and standard error.
fn boot_check(args: &[&str]) -> (Option<i32>, String, String) {
    let mut argv = vec!["boot-check"];
    argv.extend_from_slice(args);
    kbf(&argv)
}

/// The `--installer-config` flag for an `installer.toml` with `body` in a directory of its
/// own, which lasts as long as `dir`.
fn installer_config(dir: &TestDir, body: &str) -> String {
    let file = dir.join("installer.toml");
    std::fs::write(&file, body).expect("written");
    format!("--installer-config={}", file.display())
}

/// Asserts that a unit of a mode that keeps no state in the database passes without reading
/// its `installer.toml` (the file named is not there), says why it did nothing, and prints
/// nothing on standard output; and that the default `installer.toml` is not read either.
#[test]
fn a_mode_that_keeps_no_state_in_the_database_passes_without_an_installer_toml() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "boot-check-no-state");
    let absent = format!("--installer-config={}", dir.join("absent.toml").display());
    for mode in ["daemon", "storage", "storage-and-daemon"] {
        let flag = format!("--mode={mode}");
        let (code, out, err) = boot_check(&[flag.as_str(), absent.as_str()]);
        assert_eq!(code, Some(0), "{mode}: {err}");
        assert!(out.is_empty(), "{mode}: {out}");
        assert!(err.contains(NO_STATE), "{mode}: {err}");
        let (code, _, err) = boot_check(&[flag.as_str()]);
        assert_eq!(code, Some(0), "{mode}: {err}");
    }
}

/// Asserts that a frontend host whose `installer.toml` names no database passes
/// and says so: it has nothing to migrate.
#[test]
fn a_host_whose_installer_toml_names_no_database_passes() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "boot-check-no-database");
    let config = installer_config(&dir, "release_source = \"bundled\"\n");
    {
        let mode = "frontend";
        let flag = format!("--mode={mode}");
        let (code, out, err) = boot_check(&[flag.as_str(), config.as_str()]);
        assert_eq!(code, Some(0), "{mode}: {err}");
        assert!(out.is_empty(), "{mode}: {out}");
        assert!(err.contains(NO_DATABASE), "{mode}: {err}");
    }
}

/// Asserts that a frontend host whose database cannot be reached (the socket
/// directory is not there) is refused with exit 1 and the database's reason on standard
/// error, so that the unit does not start, and that the reason does not name the socket.
#[test]
fn a_host_whose_database_cannot_be_reached_is_refused_with_exit_1() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "boot-check-down");
    let url = "postgres:///kbf?host=/nonexistent-kbf-test-socket";
    let config = installer_config(&dir, &format!("database = \"{url}\"\n"));
    let said = "kbf boot-check: state schema: ";
    let socket = "nonexistent-kbf-test-socket";
    {
        let mode = "frontend";
        let flag = format!("--mode={mode}");
        let (code, out, err) = boot_check(&[flag.as_str(), config.as_str()]);
        assert_eq!(code, Some(1), "{mode}: {err}");
        assert!(out.is_empty(), "{mode}: {out}");
        assert!(err.contains(said), "{mode}: {err}");
        assert!(!err.contains(socket), "{mode}: {err}");
    }
}

/// Asserts that an `installer.toml` that is missing, is not TOML, or names a database with a
/// password in its URL is refused with exit 1 and the reason, and that the password is not
/// printed.
#[test]
fn an_installer_toml_that_cannot_be_used_is_refused_with_exit_1_and_no_secret() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "boot-check-unusable");
    let missing = format!("--installer-config={}", dir.join("absent.toml").display());
    let (code, _, err) = boot_check(&["--mode=frontend", missing.as_str()]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("absent.toml"), "{err}");
    let broken = installer_config(&dir, "database = = =");
    let (code, _, err) = boot_check(&["--mode=frontend", broken.as_str()]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("is not valid TOML"), "{err}");
    let secret = installer_config(&dir, "database = \"postgres://kbf:hunter2@db/kbf\"\n");
    let (code, _, err) = boot_check(&["--mode=frontend", secret.as_str()]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("carries a password"), "{err}");
    assert!(!err.contains("hunter2"), "{err}");
}

/// Asserts that a command line the verb does not take is a usage error (exit 2, nothing on
/// standard output, the flag or value named on standard error): no `--mode`, a mode `kbf` does
/// not have, and a flag of another verb. The unit's `RestartPreventExitStatus=2` keeps systemd
/// from restarting such a unit.
#[test]
fn a_command_line_the_verb_does_not_take_is_a_usage_error() {
    let cases = [
        (&[][..], "--mode"),
        (&["--mode=node"][..], "node"),
        (&["--mode=daemon", "--check"][..], "--check"),
        (&["--mode=daemon", "--kbf=/usr/bin/kbf"][..], "--kbf"),
    ];
    for (args, named) in cases {
        let (code, out, err) = boot_check(args);
        assert_eq!(code, Some(2), "{args:?}: {err}");
        assert!(out.is_empty(), "{args:?}: {out}");
        assert!(err.contains(named), "{args:?}: {err}");
    }
}

/// Asserts that `kbf boot-check --help` and `kbf --help` name the verb: an operator who reads
/// a unit's `ExecStartPre=` finds it.
#[test]
fn the_helps_name_the_verb() {
    let (code, out, err) = boot_check(&["--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("kbf boot-check"), "{out}");
    assert!(out.contains("--installer-config"), "{out}");
    let (code, top, err) = kbf(&["--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(top.contains("boot-check"), "{top}");
}
