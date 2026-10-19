//! `backup` and `restore` without a server: the program that dumps is played by a fake that
//! writes (or fails to write) the file `--file=` names, and everything else is the real file
//! system under a scratch directory. The round trip over a real server is
//! `real_server_round_trip` (needs `KBF_TEST_POSTGRES_URL`; the state-postgres CI job runs it).

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

/// 2027-01-15 08:00:00 UTC.
const NOW: u64 = 1_800_000_000;
const NOW_NAME: &str = "kbf-20270115T080000Z.dump";
const URL: &str = "postgres:///kbf?host=/run/postgresql";

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    kbf_host::testkit::scratch(&format!("installer-backup-{n}"))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("exists").permissions().mode() & 0o777
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .expect("lists")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn spec(dir: &Path, keep: u32) -> Spec<'_> {
    Spec {
        dir,
        url: URL,
        keep,
        pg_dump: "pg_dump",
        pg_restore: "pg_restore",
    }
}

type Dump<'a> = Box<dyn Fn(&Path) -> Result<(), String> + 'a>;

/// Plays `pg_dump` (writes through `dump`) and `pg_restore` (`--list` answers `list`).
struct Fake<'a> {
    calls: RefCell<Vec<String>>,
    dump: Dump<'a>,
    list: Result<(), String>,
}

impl<'a> Fake<'a> {
    fn dumping(content: &'static str) -> Fake<'a> {
        Fake::with(Box::new(move |f| {
            fs::write(f, content).map_err(|e| e.to_string())
        }))
    }

    fn with(dump: Dump<'a>) -> Fake<'a> {
        Fake {
            calls: RefCell::new(Vec::new()),
            dump,
            list: Ok(()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Tool for Fake<'_> {
    fn run(&self, program: &str, args: &[OsString]) -> Result<(), String> {
        let words: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        self.calls
            .borrow_mut()
            .push(format!("{program} {}", words.join(" ")));
        match program {
            "pg_dump" => {
                let file = words
                    .iter()
                    .find_map(|w| w.strip_prefix("--file="))
                    .expect("pg_dump is told where to write");
                (self.dump)(Path::new(file))
            }
            "pg_restore" if words.iter().any(|w| w == "--list") => self.list.clone(),
            "pg_restore" => Ok(()),
            other => panic!("unexpected program {other}"),
        }
    }
}

fn old_backups(dir: &Path, stamps: &[&str]) {
    for s in stamps {
        fs::write(dir.join(format!("kbf-{s}.dump")), format!("old {s}")).expect("written");
    }
}

/// Catches the file name drifting from a fixed-width UTC stamp: retention orders the files by
/// name, so a name that does not sort as time (local time, no zero padding, a wrong leap day
/// or year end) would delete the newest backup first. Also the rule for what counts as a
/// backup of this tool, which retention applies to: temp files, other files and near misses
/// are never deleted as backups.
///
/// Mutants seen red: the days-to-date conversion off by one in a leap year; `is_backup_name`
/// accepting any `kbf-*.dump`.
#[test]
fn names_are_fixed_width_utc_and_sort_as_time() {
    assert_eq!(stamp(NOW), NOW_NAME);
    assert_eq!(stamp(1_788_220_800), "kbf-20260901T000000Z.dump");
    assert_eq!(stamp(1_798_761_599), "kbf-20261231T235959Z.dump");
    assert_eq!(stamp(1_835_395_200), "kbf-20280229T000000Z.dump");
    assert_eq!(stamp(1_803_859_200), "kbf-20270301T000000Z.dump");
    assert_eq!(stamp(4_107_542_399), "kbf-21000228T235959Z.dump");
    let mut times = [NOW + 86_400 * 40, NOW, 1_788_220_800, NOW + 1];
    times.sort_unstable();
    let named: Vec<String> = times.iter().map(|t| stamp(*t)).collect();
    let mut sorted = named.clone();
    sorted.sort();
    assert_eq!(named, sorted);

    assert!(is_backup_name(NOW_NAME));
    for not in [
        ".kbf-20270115T080000Z.dump.tmp",
        "kbf-20270115T080000Z.dump.tmp",
        "kbf-20270115T080000.dump",
        "kbf-2027011T0800000Z.dump",
        "kbf-20270115t080000z.dump",
        "kbf-2027011AT080000Z.dump",
        "kbf-20270115T080000Z.dump.bak",
        "notes.txt",
        "kbf-.dump",
        "",
    ] {
        assert!(!is_backup_name(not), "{not}");
    }
}

/// Catches a backup that can be seen half written, one anyone can read, or one that is not
/// what was asked for. The dump goes to a name that is not a backup's, in a directory that is
/// already 0700 while it is written (a directory that was 0755 is tightened), the file ends
/// 0600 whatever the dumper's umask, only a rename makes it a backup, and the commands are
/// exactly the custom-format dump of the configured database and a listing of the result.
///
/// Mutants seen red: `pg_dump --file=` pointed at the final name (the dumper saw a backup
/// name); the `chmod 0600` removed (the fake writes 0644); the directory left at 0755;
/// `--format=custom` dropped; the listing skipped.
#[test]
fn a_backup_is_written_under_a_temp_name_then_renamed_owner_only() {
    let parent = scratch();
    let dir = parent.join("backups");
    fs::create_dir(&dir).expect("made");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("loose");
    let tmp = dir.join(format!(".{NOW_NAME}.tmp"));
    let seen = RefCell::new(Vec::new());
    let tool = Fake::with(Box::new(|file| {
        let d = file.parent().expect("in the directory");
        seen.borrow_mut().push((
            file.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
            mode(d),
        ));
        fs::write(file, "PGDMP").map_err(|e| e.to_string())
    }));

    let done = backup(&tool, &spec(&dir, 7), NOW).expect("backs up");

    assert_eq!(done.file, dir.join(NOW_NAME));
    assert_eq!(done.bytes, 5);
    assert!(done.removed.is_empty());
    assert_eq!(
        *seen.borrow(),
        [(format!(".{NOW_NAME}.tmp"), 0o700)],
        "dumped to a temp name, in a directory already closed"
    );
    assert_eq!(names(&dir), [NOW_NAME], "no temp file is left");
    assert_eq!(fs::read(dir.join(NOW_NAME)).expect("read"), b"PGDMP");
    assert_eq!(mode(&dir.join(NOW_NAME)), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(
        tool.calls(),
        [
            format!(
                "pg_dump --format=custom --no-comments --file={} --dbname={URL}",
                tmp.display()
            ),
            format!("pg_restore --list {}", tmp.display()),
        ]
    );
}

/// Catches a backup directory that does not exist yet not being made (the first run of the
/// timer on a new host), and made with a mode other than 0700.
///
/// Mutant seen red: `create_dir_all` with the default mode (0755).
#[test]
fn a_missing_directory_is_made_owner_only() {
    let dir = scratch().join("a").join("b");
    let tool = Fake::dumping("PGDMP");
    backup(&tool, &spec(&dir, 7), NOW).expect("backs up");
    assert_eq!(names(&dir), [NOW_NAME]);
    assert_eq!(mode(&dir), 0o700);
}

/// Catches a failed dump costing the backups that exist, or leaving a file that looks like
/// one. The dumper writes part of a file and fails: the error says why, the directory holds
/// exactly the older backups (retention did not run, even with keep = 1) and no temp file.
///
/// Mutants seen red: retention run before the dump; the temp file left behind on failure.
#[test]
fn a_failed_dump_keeps_every_older_backup_and_leaves_nothing_behind() {
    let dir = scratch();
    let olds = ["20270101T000000Z", "20270102T000000Z", "20270103T000000Z"];
    old_backups(&dir, &olds);
    let tool = Fake::with(Box::new(|f| {
        fs::write(f, "PGD").map_err(|e| e.to_string())?;
        Err("pg_dump failed (exit status: 1): connection refused".to_owned())
    }));
    let err = backup(&tool, &spec(&dir, 1), NOW).unwrap_err();
    assert!(err.contains("connection refused"), "{err}");
    assert_eq!(
        names(&dir),
        olds.map(|s| format!("kbf-{s}.dump")),
        "the old backups are all there and nothing else"
    );
}

/// Catches a dump that the dumper called a success but pg_restore cannot read being kept
/// (and so counted against the retention that deletes good backups): the listing fails, no
/// backup is made, the old ones stay, no temp file is left.
///
/// Mutant seen red: the listing's result ignored.
#[test]
fn a_dump_that_pg_restore_cannot_list_is_not_kept() {
    let dir = scratch();
    old_backups(&dir, &["20270101T000000Z"]);
    let mut tool = Fake::dumping("garbage");
    tool.list = Err("pg_restore failed (exit status: 1): not a valid archive".to_owned());
    let err = backup(&tool, &spec(&dir, 1), NOW).unwrap_err();
    assert!(err.contains("not a valid archive"), "{err}");
    assert!(err.contains("verif"), "{err}");
    assert_eq!(names(&dir), ["kbf-20270101T000000Z.dump"]);
}

/// Catches an empty file being a backup: a dumper that exits 0 and writes nothing (a full
/// disk, a wrapper script) must fail the run.
///
/// Mutant seen red: the size check removed.
#[test]
fn an_empty_dump_is_not_a_backup() {
    let dir = scratch();
    let tool = Fake::dumping("");
    let err = backup(&tool, &spec(&dir, 7), NOW).unwrap_err();
    assert!(err.contains("empty"), "{err}");
    assert!(names(&dir).is_empty(), "{:?}", names(&dir));
    // A dumper that writes no file at all is the same.
    let tool = Fake::with(Box::new(|_| Ok(())));
    assert!(backup(&tool, &spec(&dir, 7), NOW).is_err());
    assert!(names(&dir).is_empty());
}

/// Catches retention deleting the wrong files or the wrong count: with keep = 3 and five older
/// backups the new one and the two newest older ones stay, the three oldest go, and a file
/// that is not a backup of this tool is never touched. The old backups are all present while
/// the dump runs (retention follows the new backup, it does not make room for it).
///
/// Mutants seen red: the oldest kept instead of the newest (sort reversed); `keep` off by
/// one both ways; retention deleting every `.dump`; retention before the dump.
#[test]
fn retention_keeps_the_newest_n_and_touches_only_backups() {
    let dir = scratch();
    old_backups(
        &dir,
        &[
            "20270105T000000Z",
            "20270101T000000Z",
            "20270104T000000Z",
            "20270102T000000Z",
            "20270103T000000Z",
        ],
    );
    fs::write(dir.join("notes.txt"), "keep me").expect("written");
    fs::write(dir.join("other.dump"), "keep me too").expect("written");
    let before = names(&dir).len();
    let tool = Fake::with(Box::new(|f| {
        assert_eq!(
            names(f.parent().expect("dir")).len(),
            before,
            "retention must not run before the dump"
        );
        fs::write(f, "PGDMP").map_err(|e| e.to_string())
    }));

    let done = backup(&tool, &spec(&dir, 3), NOW).expect("backs up");

    assert_eq!(
        names(&dir),
        [
            "kbf-20270104T000000Z.dump",
            "kbf-20270105T000000Z.dump",
            NOW_NAME,
            "notes.txt",
            "other.dump",
        ]
    );
    assert_eq!(
        done.removed,
        [
            "kbf-20270101T000000Z.dump",
            "kbf-20270102T000000Z.dump",
            "kbf-20270103T000000Z.dump",
        ]
    );

    // keep = 1 leaves exactly the new one.
    backup(&Fake::dumping("PGDMP"), &spec(&dir, 1), NOW + 60).expect("again");
    let left: Vec<String> = names(&dir)
        .into_iter()
        .filter(|n| is_backup_name(n))
        .collect();
    assert_eq!(left, ["kbf-20270115T080100Z.dump"]);
}

/// Catches fewer backups than the count existing being deleted: with keep = 7 and two older
/// backups nothing is removed.
///
/// Mutant seen red: retention removing `len - keep + 1`.
#[test]
fn fewer_backups_than_keep_are_all_kept() {
    let dir = scratch();
    old_backups(&dir, &["20270101T000000Z", "20270102T000000Z"]);
    let done = backup(&Fake::dumping("PGDMP"), &spec(&dir, 7), NOW).expect("backs up");
    assert!(done.removed.is_empty());
    assert_eq!(names(&dir).len(), 3);
}

/// Catches the new backup being deleted when the clock is behind the newest older backup (a
/// clock set back): retention counts the new one as kept whatever its name sorts as.
///
/// Mutant seen red: the new file counted among the candidates for removal.
#[test]
fn the_new_backup_is_never_the_one_retention_removes() {
    let dir = scratch();
    old_backups(&dir, &["20270301T000000Z", "20270302T000000Z"]);
    let done = backup(&Fake::dumping("PGDMP"), &spec(&dir, 2), NOW).expect("backs up");
    assert_eq!(
        names(&dir),
        [NOW_NAME, "kbf-20270302T000000Z.dump"],
        "the new one and the newest older one stay"
    );
    assert_eq!(done.removed, ["kbf-20270301T000000Z.dump"]);
}

/// Catches a crashed earlier run's temp file lingering for ever (it is a full database dump
/// that no retention would ever count) and a temp file being taken for a backup.
///
/// Mutant seen red: stale temp files left in place.
#[test]
fn a_stale_temp_file_of_a_crashed_run_is_removed() {
    let dir = scratch();
    fs::write(dir.join(".kbf-20270114T080000Z.dump.tmp"), "half").expect("written");
    old_backups(&dir, &["20270113T080000Z"]);
    let done = backup(&Fake::dumping("PGDMP"), &spec(&dir, 7), NOW).expect("backs up");
    assert_eq!(names(&dir), ["kbf-20270113T080000Z.dump", NOW_NAME]);
    assert!(
        done.removed.is_empty(),
        "a temp file is not a retained backup"
    );
}

/// Catches a backup replacing another: two runs in the same second (a manual run beside the
/// timer) must fail the second, with the first untouched.
///
/// Mutant seen red: `rename` over an existing backup.
#[test]
fn a_backup_of_the_same_second_is_never_replaced() {
    let dir = scratch();
    fs::write(dir.join(NOW_NAME), "first").expect("written");
    let err = backup(&Fake::dumping("second"), &spec(&dir, 7), NOW).unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(fs::read(dir.join(NOW_NAME)).expect("read"), b"first");
    assert_eq!(names(&dir), [NOW_NAME]);
}

/// Catches a backup directory that is a link, or a file, being used: a link could point the
/// dumps (and the deletions) anywhere. No program runs.
///
/// Mutant seen red: `metadata` (follows links) in place of `symlink_metadata`.
#[test]
fn the_directory_must_be_a_real_directory() {
    let parent = scratch();
    let real = parent.join("real");
    fs::create_dir(&real).expect("made");
    let link = parent.join("link");
    symlink(&real, &link).expect("linked");
    let file = parent.join("file");
    fs::write(&file, "x").expect("written");
    for bad in [&link, &file] {
        let tool = Fake::dumping("PGDMP");
        let err = backup(&tool, &spec(bad, 7), NOW).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert!(tool.calls().is_empty());
    }
    assert!(names(&real).is_empty());
}

/// Catches the restore command drifting: it must be a single transaction that stops at the
/// first error (a half-restored database is worse than none), take no owner or privilege
/// from the dump (the role that restores owns the tables, as `setup` made them), and name
/// the database and the file. A missing file, a link or a directory is refused before any
/// program runs; the tool's failure is the verb's.
///
/// Mutants seen red: `--single-transaction` or `--exit-on-error` dropped; `--no-owner`
/// dropped; the existence check removed.
#[test]
fn restore_runs_pg_restore_in_one_transaction_and_refuses_what_is_not_a_file() {
    let dir = scratch();
    let file = dir.join(NOW_NAME);
    fs::write(&file, "PGDMP").expect("written");
    let tool = Fake::dumping("");
    restore(&tool, "pg_restore", URL, &file).expect("restores");
    assert_eq!(
        tool.calls(),
        [format!(
            "pg_restore --exit-on-error --single-transaction --no-owner --no-privileges --dbname={URL} {}",
            file.display()
        )]
    );

    let link = dir.join("link.dump");
    symlink(&file, &link).expect("linked");
    for bad in [dir.join("missing.dump"), link, dir.clone()] {
        let tool = Fake::dumping("");
        let err = restore(&tool, "pg_restore", URL, &bad).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        assert!(tool.calls().is_empty());
    }

    struct Failing;
    impl Tool for Failing {
        fn run(&self, _: &str, _: &[OsString]) -> Result<(), String> {
            Err("pg_restore failed (exit status: 1): relation exists".to_owned())
        }
    }
    let err = restore(&Failing, "pg_restore", URL, &file).unwrap_err();
    assert!(err.contains("relation exists"), "{err}");
}

/// Catches the real runner mishandling a program: success is Ok, a failure carries the exit
/// status and the end of what the program wrote on standard error (the one place the cause is
/// said), a program that is not there is an error naming it, and standard input is closed.
///
/// Mutants seen red: a nonzero exit treated as success; the stderr dropped from the message;
/// the whole of a long stderr kept.
#[test]
fn the_process_runner_reports_what_the_program_said() {
    assert_eq!(Process.run("true", &[]), Ok(()));
    let err = Process.run("false", &[]).unwrap_err();
    assert!(err.starts_with("false failed (exit status: 1)"), "{err}");
    // `ls` of a path that is not there says so on standard error and exits 2.
    let err = Process
        .run("ls", &["/kbf-no-such-directory-for-the-test".into()])
        .unwrap_err();
    assert!(
        err.starts_with("ls failed (exit status: 2): ")
            && err.contains("kbf-no-such-directory-for-the-test"),
        "{err}"
    );
    // `cat` with nothing to read ends at once only when standard input is closed.
    assert_eq!(Process.run("cat", &[]), Ok(()));
    // A path longer than a name may be: the error line quotes it, and the tail is bounded.
    let long = format!("/{}", "x".repeat(3000));
    let err = Process.run("ls", &[long.into()]).unwrap_err();
    assert!(err.len() < 2200, "{}", err.len());
    let err = Process.run("kbf-no-such-program-here", &[]).unwrap_err();
    assert!(err.contains("kbf-no-such-program-here"), "{err}");
}

fn conf(body: &str) -> PathBuf {
    let path = scratch().join("installer.toml");
    fs::write(&path, body).expect("written");
    path
}

/// Catches the settings read wrongly: the URL and the count come from `installer.toml`, the
/// count defaults to 7, and a file without a database, with a password in the URL, or with a
/// count out of bounds is refused with a line that never quotes the URL.
///
/// Mutants seen red: the default changed; the password check dropped.
#[test]
fn settings_come_from_installer_toml_and_refuse_a_password() {
    let ok = conf(&format!("database = \"{URL}\"\n"));
    assert_eq!(
        Settings::read(&ok),
        Ok(Settings {
            url: URL.to_owned(),
            keep: 7
        })
    );
    let kept = conf(&format!("database = \"{URL}\"\nbackup_keep = 30\n"));
    assert_eq!(Settings::read(&kept).expect("reads").keep, 30);

    for body in [
        "release_source = \"frontend\"\n".to_owned(),
        format!("database = \"{URL}\"\nbackup_keep = 0\n"),
        "database = = =".to_owned(),
    ] {
        assert!(Settings::read(&conf(&body)).is_err(), "{body}");
    }
    let err = Settings::read(&conf("database = \"postgres://kbf:hunter2@db/kbf\"\n")).unwrap_err();
    assert!(
        err.contains("password") && !err.contains("hunter2"),
        "{err}"
    );
    let err = Settings::read(&scratch().join("none.toml")).unwrap_err();
    assert!(err.contains("none.toml"), "{err}");
}

// ---- against a server -----------------------------------------------------------------

struct Server {
    host: String,
    port: u16,
    admin: postgres::Client,
}

impl Server {
    fn connect() -> Server {
        let url = std::env::var("KBF_TEST_POSTGRES_URL")
            .expect("KBF_TEST_POSTGRES_URL names a test database");
        let config: postgres::Config = url.parse().expect("a postgres URL");
        let host = match config.get_hosts().first() {
            Some(postgres::config::Host::Tcp(h)) => h.clone(),
            other => panic!("the test server is reached over TCP: {other:?}"),
        };
        let port = config.get_ports().first().copied().unwrap_or(5432);
        let admin = config.connect(postgres::NoTls).expect("the server answers");
        Server { host, port, admin }
    }

    /// A URL for `role` and `database` with no password: the test server trusts its clients.
    fn url(&self, role: &str, database: &str) -> String {
        format!("postgres://{role}@{}:{}/{database}", self.host, self.port)
    }
}

/// Where `pg_dump` and `pg_restore` are: `KBF_TEST_PG_BIN` (a directory) when the job sets
/// it, because the tools must be as new as the server; else the PATH's.
fn pg_tool(name: &str) -> String {
    match std::env::var("KBF_TEST_PG_BIN") {
        Ok(dir) => format!("{dir}/{name}"),
        Err(_) => name.to_owned(),
    }
}

/// Catches a backup that cannot be restored, or one that restores to something kbf refuses.
/// On a real server, as the database's own role (not a superuser, as in production, where
/// the role owns the database and nothing else): the schema is made the way `setup` makes
/// it, rows are written through kbf's store, `backup` runs against the server, the file is
/// restored into a second fresh database, and kbf's store opens that database (its schema
/// check accepts it) and reads every row back with the same value, including a row written
/// after a second `backup` and a restore of the newer file.
///
/// Mutants seen red: `--no-comments` dropped (restoring as a non-superuser fails on
/// the plpgsql comment); `--single-transaction` dropped is not visible here (see the unit
/// test of the arguments); the dump taken of another database (rows missing).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn real_server_round_trip() {
    use kbf_state::postgres::{PostgresConfig, PostgresStore};
    use kbf_state::{Cond, Store, Txn};

    use crate::database::Database;

    let mut server = Server::connect();
    let tag = format!("b6_{}", std::process::id());
    let role = format!("kbf_{tag}");
    let (source, target, later) = (
        format!("src_{tag}"),
        format!("dst_{tag}"),
        format!("lat_{tag}"),
    );
    struct Cleanup(Vec<String>, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let mut server = Server::connect();
            for db in &self.0 {
                let _ = server
                    .admin
                    .batch_execute(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"));
            }
            let _ = server
                .admin
                .batch_execute(&format!("DROP ROLE IF EXISTS {}", self.1));
        }
    }
    let _cleanup = Cleanup(
        vec![source.clone(), target.clone(), later.clone()],
        role.clone(),
    );
    server
        .admin
        .batch_execute(&format!(
            "CREATE ROLE {role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE"
        ))
        .expect("role");
    for db in [&source, &target, &later] {
        server
            .admin
            .batch_execute(&format!("CREATE DATABASE {db} OWNER {role}"))
            .expect("database");
    }

    let config = |db: &str| PostgresConfig::parse(&server.url(&role, db)).expect("parses");
    Database::new(config(&source), kbf_state::postgres::schema::embedded())
        .setup()
        .expect("setup makes the schema");
    let store = PostgresStore::open(&config(&source)).expect("opens");
    let rows = [("map/a", "one"), ("done/b", "two"), ("desired", "three")];
    for (k, v) in rows {
        store
            .txn(Txn::new().put(k, v, Cond::Any))
            .expect("reachable")
            .expect("commits");
    }

    let dir = scratch().join("backups");
    let src_url = server.url(&role, &source);
    let spec = Spec {
        dir: &dir,
        url: &src_url,
        keep: 7,
        pg_dump: &pg_tool("pg_dump"),
        pg_restore: &pg_tool("pg_restore"),
    };
    let first = backup(&Process, &spec, NOW).expect("the first backup succeeds");
    assert_eq!(mode(&first.file), 0o600);

    restore(
        &Process,
        &pg_tool("pg_restore"),
        &server.url(&role, &target),
        &first.file,
    )
    .expect("the backup restores into a fresh database as the database's own role");
    let restored = PostgresStore::open(&config(&target))
        .expect("kbf opens the restored database: its schema check accepts it");
    for (k, v) in rows {
        let got = restored.get(k).expect("reads").expect("the row is there");
        assert_eq!(got.value, v.as_bytes(), "{k}");
    }
    assert_eq!(
        kbf_state::postgres::schema::current(&config(&target)),
        Ok(kbf_state::postgres::schema::latest())
    );

    // A restore into a database that already holds the schema fails, whole: nothing of the
    // dump is merged into live state.
    assert!(
        restore(
            &Process,
            &pg_tool("pg_restore"),
            &server.url(&role, &target),
            &first.file
        )
        .is_err()
    );

    // A newer row, a second backup, and a restore of that one.
    store
        .txn(Txn::new().put("late", "four", Cond::Any))
        .expect("reachable")
        .expect("commits");
    let second = backup(&Process, &spec, NOW + 60).expect("the second backup succeeds");
    restore(
        &Process,
        &pg_tool("pg_restore"),
        &server.url(&role, &later),
        &second.file,
    )
    .expect("restores");
    let newer = PostgresStore::open(&config(&later)).expect("opens");
    assert_eq!(
        newer.get("late").expect("reads").expect("present").value,
        b"four"
    );
    assert!(restored.get("late").expect("reads").is_none());
}
