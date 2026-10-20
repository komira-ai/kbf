//! `backup` and `restore` through the CLI layer (`run_backup`, `run_restore`).

use super::*;

// ---- backup and restore ----------------------------------------------------------------

const DB_URL: &str = "postgres:///kbf?host=/run/postgresql";

/// A host root with an `installer.toml` holding `body`.
fn root_with_config(name: &str, body: &str) -> PathBuf {
    let root = kbf_host::testkit::scratch(name);
    fs::create_dir_all(root.join("etc/kbf")).expect("etc/kbf");
    fs::write(root.join("etc/kbf/installer.toml"), body).expect("installer.toml");
    root
}

fn db_config() -> String {
    format!("database = \"{DB_URL}\"\nbackup_keep = 2\n")
}

/// Plays `pg_dump` (writes the file `--file=` names) and `pg_restore`, recording each call.
struct Fakes(std::cell::RefCell<Vec<String>>);

impl backup::Tool for Fakes {
    fn run(&self, program: &str, args: &[std::ffi::OsString]) -> Result<(), String> {
        let words: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        self.0
            .borrow_mut()
            .push(format!("{program} {}", words.join(" ")));
        if let Some(file) = words.iter().find_map(|w| w.strip_prefix("--file=")) {
            fs::write(file, "PGDMP").map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn backup_args(argv: &[&str]) -> crate::cli::BackupArgs {
    match parse(argv).verb {
        Verb::Backup(a) => a,
        _ => panic!("not a backup"),
    }
}

fn restore_args(argv: &[&str]) -> crate::cli::RestoreArgs {
    match parse(argv).verb {
        Verb::Restore(a) => a,
        _ => panic!("not a restore"),
    }
}

/// Catches `backup` and `restore` acting without `--apply`, or being refused for not being
/// root (they run as `kbf-server`, whose role is the database's, never as root): a dry run as
/// an ordinary user says what it would do, exits 0, and makes no directory and no file.
///
/// Mutants seen red: the dry-run check removed (the directory appears); `needs_root` true.
#[test]
fn a_dry_run_says_what_it_would_do_and_touches_nothing() {
    let root = root_with_config("app-backup-dry", &db_config());
    let r = root.to_str().expect("text");
    let (exit, out, err) = run_as(1000, &["--root", r, "backup"]);
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("backup: dry run: would dump the state database into")
            && err.contains("var/lib/kbf-server/backups, keeping the newest 2"),
        "{err}"
    );
    assert!(!root.join("var").exists(), "a dry run made a directory");

    let (exit, _, err) = run_as(
        1000,
        &[
            "--root",
            r,
            "restore",
            "--file",
            "/var/lib/kbf-server/backups/kbf-x.dump",
        ],
    );
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(err.contains("restore: dry run: would restore"), "{err}");
}

/// Catches the verb dumping a database other than `installer.toml`'s, into a directory other
/// than the flag's (under the host root), with the keep count of the file, and not telling
/// what it wrote: the one `pg_dump` call names the file's URL, the file is in the directory,
/// and standard error says so.
///
/// Mutant seen red: the URL taken from a constant in place of the file.
#[test]
fn backup_dumps_the_database_the_installer_config_names() {
    let root = root_with_config("app-backup-run", &db_config());
    let r = root.to_str().expect("text");
    let args = backup_args(&["--root", r, "--apply", "backup", "--dir", "/srv/b"]);
    let tool = Fakes(Default::default());
    let mut said = Vec::new();
    run_backup(&tool, root.clone(), true, &args, &mut said).expect("backs up");
    let calls = tool.0.borrow();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with("pg_dump --format=custom --no-comments --file=")
            && calls[0].ends_with(&format!("--dbname={DB_URL}")),
        "{}",
        calls[0]
    );
    let made: Vec<String> = fs::read_dir(root.join("srv/b"))
        .expect("the directory is under the root")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(made.len(), 1, "{made:?}");
    assert!(backup::is_backup_name(&made[0]), "{made:?}");
    let said = String::from_utf8(said).expect("text");
    assert!(
        said.contains("backup: wrote") && said.contains(&made[0]),
        "{said}"
    );
    assert!(said.contains("(5 bytes); removed 0 old"), "{said}");
}

/// Catches a failing backup ending in success (a unit that never shows failed), and the
/// settings' faults not being exit 1: no installer.toml, none naming a database, a password
/// in the URL, and a `pg_dump` that cannot be started each fail, and none of them prints the
/// URL.
///
/// Mutant seen red: the tool's error swallowed.
#[test]
fn a_backup_that_cannot_be_made_fails_with_exit_1_and_says_why() {
    let none = kbf_host::testkit::scratch("app-backup-none");
    let (exit, _, err) = run_as(
        1000,
        &["--root", none.to_str().expect("text"), "--apply", "backup"],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("installer.toml"), "{err}");

    let nodb = root_with_config("app-backup-nodb", "release_source = \"frontend\"\n");
    let (exit, _, err) = run_as(
        1000,
        &["--root", nodb.to_str().expect("text"), "--apply", "backup"],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("no `database"), "{err}");

    let secret = root_with_config(
        "app-backup-secret",
        "database = \"postgres://kbf:hunter2@db/kbf\"\n",
    );
    let (exit, _, err) = run_as(
        1000,
        &[
            "--root",
            secret.to_str().expect("text"),
            "--apply",
            "backup",
        ],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(
        err.contains("password") && !err.contains("hunter2"),
        "{err}"
    );

    let root = root_with_config("app-backup-nopg", &db_config());
    let (exit, _, err) = run_as(
        1000,
        &[
            "--root",
            root.to_str().expect("text"),
            "--apply",
            "backup",
            "--pg-dump",
            "kbf-no-such-program-here",
        ],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("cannot run kbf-no-such-program-here"), "{err}");
    let (exit, out, _) = run_as(
        1000,
        &[
            "--json",
            "--root",
            root.to_str().expect("text"),
            "--apply",
            "backup",
            "--pg-dump",
            "kbf-no-such-program-here",
        ],
    );
    assert_eq!(exit, Exit::Failed);
    let v: Value = serde_json::from_str(out.trim_end()).expect("one object");
    assert_eq!(
        (v["verb"].as_str(), v["error"].as_str()),
        (Some("backup"), Some("failed"))
    );
}

/// Catches `restore` going to the wrong database or taking a password: the file's URL is the
/// default, `--database` replaces it (a scratch database for a restore test), a password in
/// either is refused without quoting it, and the file named is the one under the host root.
///
/// Mutants seen red: `--database` ignored; its password check dropped.
#[test]
fn restore_names_the_database_from_the_config_or_the_flag() {
    let root = root_with_config("app-restore", &db_config());
    let r = root.to_str().expect("text");
    fs::create_dir_all(root.join("b")).expect("b");
    fs::write(root.join("b/kbf-x.dump"), "PGDMP").expect("dump");

    let args = restore_args(&["--root", r, "restore", "--file", "/b/kbf-x.dump"]);
    let tool = Fakes(Default::default());
    let mut said = Vec::new();
    run_restore(&tool, root.clone(), true, &args, &mut said).expect("restores");
    assert_eq!(
        tool.0.borrow()[0],
        format!(
            "pg_restore --exit-on-error --single-transaction --no-owner --no-privileges --dbname={DB_URL} {}",
            root.join("b/kbf-x.dump").display()
        )
    );
    assert!(
        String::from_utf8(said)
            .expect("text")
            .contains("restore: restored")
    );

    let scratch_db = "postgres:///kbf_restore_test?host=/run/postgresql";
    let args = restore_args(&[
        "--root",
        r,
        "restore",
        "--file",
        "/b/kbf-x.dump",
        "--database",
        scratch_db,
    ]);
    let tool = Fakes(Default::default());
    run_restore(&tool, root.clone(), true, &args, &mut Vec::new()).expect("restores");
    assert!(
        tool.0.borrow()[0].contains(&format!("--dbname={scratch_db} ")),
        "{:?}",
        tool.0
    );

    let (exit, _, err) = run_as(
        1000,
        &[
            "--root",
            r,
            "--apply",
            "restore",
            "--file",
            "/b/kbf-x.dump",
            "--database",
            "postgres://kbf:hunter2@db/kbf",
        ],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(
        err.contains("password") && !err.contains("hunter2"),
        "{err}"
    );
    let (exit, _, err) = run_as(
        1000,
        &[
            "--root",
            r,
            "--apply",
            "restore",
            "--file",
            "/b/none.dump",
            "--pg-restore",
            "kbf-none",
        ],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("not a regular file"), "{err}");
    let (exit, _, err) = run_as(
        1000,
        &[
            "--root",
            r,
            "--apply",
            "restore",
            "--file",
            "/b/kbf-x.dump",
            "--pg-restore",
            "kbf-none",
        ],
    );
    assert_eq!(exit, Exit::Failed);
    assert!(err.contains("cannot run kbf-none"), "{err}");
}

#[path = "app_restore_tests.rs"]
mod restore_scratch;
