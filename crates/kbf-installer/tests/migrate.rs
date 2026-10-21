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

use kbf_installer::layout::{Layout, Platform};
use kbf_installer::migrate::journal::{Journal, Phase};
use kbf_installer::migrate::roots::Roots;
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
    let old = old_state(&root);
    fs::create_dir_all(&old).expect("old state dir");
    fs::write(old.join("state.json"), state).expect("state");
    // Whatever the umask of whoever runs the test: root's files, as the migration demands.
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).expect("mode");
    fs::set_permissions(old.join("state.json"), fs::Permissions::from_mode(0o600)).expect("mode");
    root
}

/// The host as the binary lays it out: the platform it was built for.
fn host(root: &Path) -> Layout {
    Layout::new(
        kbf_host::Root::new(root).expect("absolute"),
        Platform::current(),
    )
}

/// The old tools' state directory of this platform (`var/lib/kbf-deploy` on Linux,
/// `var/db/kbf-deploy` on macOS), under `root`.
fn old_state(root: &Path) -> PathBuf {
    host(root).join(&Roots::new(Platform::current()).old_state)
}

/// The installer's own state directory of this platform, under `root`.
fn state_dir(root: &Path) -> PathBuf {
    let host = host(root);
    host.join(&host.state_dir())
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
    fs::remove_file(old_state(&root).join("state.json")).expect("rm");
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
    assert!(!state_dir(&root).exists());
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
    assert!(!state_dir(&root).exists());
    assert!(!old_state(&root).join("MIGRATED").exists());
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

/// A host whose installer was adopted and put back, as the binary lays it out: only the
/// journal, in the platform's state directory (`/var/lib/kbf-installer`, `/var/db/kbf-installer`
/// on a Mac), in `phase`.
fn journalled_host(name: &str, phase: Phase) -> PathBuf {
    let root = kbf_host::testkit::scratch(&format!("uninstall-bin-{name}"));
    fs::create_dir_all(root.join("run")).expect("run");
    let layout = Layout::new(
        kbf_host::Root::new(&root).expect("absolute"),
        Platform::current(),
    );
    let dir = layout.join(&layout.state_dir());
    fs::create_dir_all(&dir).expect("state dir");
    // Whatever the umask of whoever runs the test: root's files, as the check demands.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("mode");
    let mut journal = Journal::new(Vec::new(), None);
    journal.phase = phase;
    fs::write(dir.join("migration.json"), journal.render()).expect("journal");
    fs::set_permissions(
        dir.join("migration.json"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("mode");
    root
}

fn uninstall(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--root")
        .arg(root)
        .arg("uninstall")
        .args(args)
        .output()
        .expect("runs")
}

fn state_names(root: &Path) -> Vec<String> {
    let layout = Layout::new(
        kbf_host::Root::new(root).expect("absolute"),
        Platform::current(),
    );
    let mut names: Vec<String> = fs::read_dir(layout.join(&layout.state_dir()))
        .expect("state dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches `uninstall` not wired into the shipped binary or not following the dry-run rule:
/// on a host whose migration was rolled back, a run without `--apply` prints the journal's
/// archive as a `DRY-RUN:` line on standard output, exits 0 and changes nothing; with
/// `--apply` it exits 0 and the journal is kept under a `.rolled-back-` name and gone from its
/// own; run again it says there is nothing to uninstall and exits 0.
///
/// Mutants seen red: the `Uninstall` arm of the dispatcher removed; the dry-run `continue` removed.
/// Not run as a mutant: `--apply` ignored.
#[test]
fn uninstall_is_a_dry_run_until_apply_and_keeps_the_journal() {
    let root = journalled_host("dry", Phase::RolledBack);
    let before = state_names(&root);
    let out = uninstall(&root, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown = text(&out.stdout);
    assert!(
        shown.contains("DRY-RUN: archive the journal")
            && shown.contains("migration.json.rolled-back-"),
        "{shown}"
    );
    assert_eq!(state_names(&root), before);

    let out = uninstall(&root, &["--apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let after = state_names(&root);
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(
        after[0].starts_with("migration.json.rolled-back-"),
        "{after:?}"
    );

    let out = uninstall(&root, &["--apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("nothing to uninstall"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(state_names(&root), after);
}

/// Catches the exit codes of `uninstall` at the command line: a host whose migration is done
/// (the installer manages it) is refused with exit 5, names the way out and changes nothing;
/// a unit name that is a path is a usage error (2).
///
/// Mutant seen red: the phase check removed (the journal of a `Done` host archived).
#[test]
fn uninstall_refuses_a_managed_host_with_exit_5() {
    let root = journalled_host("done", Phase::Done);
    let before = state_names(&root);
    let out = uninstall(&root, &["--apply"]);
    assert_eq!(out.status.code(), Some(5), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("migrate --rollback"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(state_names(&root), before);
    let out = uninstall(&root, &["--installer-unit", "../x.service"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
}
