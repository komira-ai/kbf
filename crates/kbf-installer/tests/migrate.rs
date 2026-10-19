//! `kbf-installer migrate` as the operator runs it, on a scratch root: the command line,
//! the exit codes and what is printed. Nothing here needs root: under `--root` the tree is
//! the caller's own, and no unit is named, so no service is touched.
//!
//! These tests do not get as far as running an old release's binary (the version probe):
//! that needs a program that prints `--version --json`, and the repository's interpreter
//! budget allows no script. The probe, and the migration after it, are the library's tests
//! (`src/migrate/`), which play it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_installer::testkit::{A, B};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

/// The user running the tests, which the probe then runs as (no switch needed).
fn me() -> String {
    let out = Command::new("/usr/bin/id").arg("-un").output().expect("id");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn old_host(name: &str) -> PathBuf {
    // Not `host_root`: that makes the installer's directories, and a host to migrate has none.
    let root = kbf_host::testkit::scratch(&format!("migrate-bin-{name}"));
    fs::create_dir_all(root.join("run")).expect("run");
    for commit in [A, B] {
        let dir = root.join("usr/local/kbf/releases").join(commit);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), format!("kbf {commit}\n")).expect("binary");
        fs::set_permissions(dir.join("kbf"), fs::Permissions::from_mode(0o644)).expect("mode");
    }
    let tree = root.join("usr/local/kbf");
    std::os::unix::fs::symlink(format!("releases/{A}"), tree.join("current")).expect("link");
    std::os::unix::fs::symlink(format!("releases/{B}"), tree.join("previous")).expect("link");
    // The old tools' state: schema 1, which the installer's own reader would refuse.
    let state = format!(
        "{{\"schema\": 1, \"current\": \"{A}\", \"previous\": \"{B}\", \"staged\": null, \
         \"pending_confirm\": null, \"order_floor\": 0}}"
    );
    let old = root.join("var/lib/kbf-deploy");
    fs::create_dir_all(&old).expect("old state dir");
    fs::write(old.join("state.json"), state).expect("state");
    // Whatever the umask of whoever runs the test: root's files, as the migration demands.
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).expect("mode");
    fs::set_permissions(old.join("state.json"), fs::Permissions::from_mode(0o600)).expect("mode");
    root
}

fn migrate(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--root")
        .arg(root)
        .arg("migrate")
        .args(["--fetch-user", &me()])
        .args(args)
        .output()
        .expect("runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Catches a verb that is not wired, or that changes a host with nothing to migrate: a host
/// with no old state says so on standard output and exits 0, as a dry run and with
/// `--apply`, and writes nothing; `--rollback` of what was never migrated is exit 5.
#[test]
fn a_host_with_no_old_layout_has_nothing_to_migrate_and_nothing_to_roll_back() {
    let root = old_host("nothing");
    fs::remove_file(root.join("var/lib/kbf-deploy/state.json")).expect("rm");
    for args in [&[][..], &["--apply"], &["--min-generation", "5", "--apply"]] {
        let out = migrate(&root, args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stdout).contains("nothing to migrate"),
            "{args:?}: {}",
            text(&out.stdout)
        );
    }
    assert!(!root.join("var/lib/kbf-installer").exists());
    let out = migrate(&root, &["--rollback", "--apply"]);
    assert_eq!(out.status.code(), Some(5), "{}", text(&out.stderr));
}

/// Catches the exit codes of the contract at the command line: a frontend source with no
/// frontend to ask and no flag (5, naming the flag), an unknown probe user (2), a unit name
/// that is a path (2), and a rollback of what was never migrated (5). Each stops before
/// anything is changed.
#[test]
fn refusals_exit_with_the_contract_codes() {
    let root = old_host("refusals");
    let out = migrate(&root, &["--release-source", "frontend"]);
    assert_eq!(out.status.code(), Some(5), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("--min-generation"),
        "{}",
        text(&out.stderr)
    );

    let out = migrate(&root, &["--rollback", "--apply"]);
    assert_eq!(out.status.code(), Some(5), "{}", text(&out.stderr));

    let out = Command::new(BIN)
        .arg("--root")
        .arg(&root)
        .args(["migrate", "--fetch-user", "no-such-user-kbf-test"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("no user"),
        "{}",
        text(&out.stderr)
    );

    let out = migrate(&root, &["--unit", "../x.service"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(!root.join("var/lib/kbf-installer").exists());
    assert!(!root.join("var/lib/kbf-deploy/MIGRATED").exists());
}

/// Catches a `--root` that is not a path from `/` being accepted: it is exit 1 with the
/// reason, before any host is read.
#[test]
fn a_root_that_is_not_absolute_is_a_failure() {
    let out = Command::new(BIN)
        .args(["--root", "relative/path", "migrate", "--fetch-user", &me()])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).starts_with("kbf-installer: "),
        "{}",
        text(&out.stderr)
    );
}

/// Catches the Postgres crossing missing from the binary, or reaching a node: on a host of
/// the old layout `--mode frontend` and `--mode all-in-one` are refused (a dry run and an
/// apply) with exit 2, naming the state database and what must exist, before any program
/// runs or any file is written; a node mode is not asked for a database at all and goes on to
/// the next step (the old release's rules and version probe, where it stops here: the old
/// binary cannot be run, so the exit is not 2). Needs no Postgres on the machine: the host's
/// socket is not there, or has no `kbf` role for this user.
///
/// Mutants seen red: the refusal swallowed; the database asked after the version probe; the
/// refusal ending with exit 1. On a machine with a Postgres on its socket the refusal is
/// the server's (no `kbf` role for this user), still exit 2.
#[test]
fn the_database_modes_need_a_postgres_and_a_node_is_not_asked() {
    let root = old_host("modes");
    for mode in ["frontend", "all-in-one"] {
        for apply in [&[][..], &["--apply"]] {
            let mut args = vec!["--mode", mode];
            args.extend_from_slice(apply);
            let out = migrate(&root, &args);
            assert_eq!(
                out.status.code(),
                Some(2),
                "{args:?}: {}",
                text(&out.stderr)
            );
            let said = text(&out.stderr);
            assert!(
                said.contains("state database") && said.contains("creates none of them"),
                "{args:?}: {said}"
            );
        }
    }
    for mode in ["storage", "daemon", "storage-and-daemon"] {
        let out = migrate(&root, &["--mode", mode]);
        let said = text(&out.stderr);
        assert!(!said.contains("state database"), "{mode}: {said}");
        assert_ne!(out.status.code(), Some(2), "{mode}: {said}");
    }
    assert!(!root.join("var/lib/kbf-installer").exists());
    assert!(!root.join("var/lib/kbf-deploy/MIGRATED").exists());
}
