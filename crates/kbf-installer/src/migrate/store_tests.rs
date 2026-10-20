//! The first crossing of a frontend or all-in-one host onto Postgres: the schema step, the
//! switch of the units' `--state-store`, the refusals, the dry run, the rollback and the re-run
//! after a crash. The database is played by a fake that records what it was asked and shares
//! the rig's event list, so an order against the services can be asserted; no server is
//! needed. The real database is asked only where no server answers.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::rc::Rc;

use kbf_state::postgres::PostgresConfig;
use kbf_state::postgres::schema::{Outcome as SchemaOutcome, SchemaError};
use serde_json::json;

use super::rig::*;
use super::store::{FRESH_START, applied};
use super::*;
use crate::fixtures::{FakeRunner, ran, version_json};
use crate::testkit::{A, B};

fn never() -> FnVerifier {
    FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())))
}

/// What the fake database remembers.
struct Db {
    schema: Option<u32>,
    newest: u32,
    down: Option<String>,
    refuses: Option<String>,
    calls: Vec<&'static str>,
}

/// A state database that answers as a test says, records each call, and puts "database
/// setup" in the rig's list of service events when its schema step runs.
#[derive(Clone)]
struct FakeDb {
    db: Rc<RefCell<Db>>,
    events: Rc<RefCell<Vec<String>>>,
}

impl FakeDb {
    fn new(rig: &Rig, schema: Option<u32>) -> FakeDb {
        FakeDb {
            db: Rc::new(RefCell::new(Db {
                schema,
                newest: 3,
                down: None,
                refuses: None,
                calls: Vec::new(),
            })),
            events: rig.events.clone(),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.db.borrow().calls.clone()
    }

    fn schema(&self) -> Option<u32> {
        self.db.borrow().schema
    }
}

impl StateDatabase for FakeDb {
    fn plan(&self) -> Result<(Option<u32>, u32), String> {
        let mut db = self.db.borrow_mut();
        db.calls.push("plan");
        match &db.down {
            Some(why) => Err(why.clone()),
            None => Ok((db.schema, db.newest)),
        }
    }

    fn setup(&self) -> Result<String, String> {
        let mut db = self.db.borrow_mut();
        db.calls.push("setup");
        if let Some(why) = &db.refuses {
            return Err(why.clone());
        }
        let (from, newest) = (db.schema, db.newest);
        db.schema = Some(newest);
        self.events.borrow_mut().push("database setup".to_owned());
        let ran = if from == Some(newest) {
            Vec::new()
        } else {
            vec![newest]
        };
        Ok(SchemaOutcome {
            from,
            to: newest,
            ran,
        }
        .to_string())
    }
}

/// The all-in-one unit of the old layout: the binary from the old link, the state store a
/// SQLite file on a continuation line.
const AIO_UNIT: &str = "[Unit]\nDescription=kbf all-in-one\n\n[Service]\n\
# --state-store=sqlite:///var/lib/not/a/flag.db is a comment\n\
ExecStart=/usr/local/kbf/current/kbf all-in-one \\\n    --state-store=sqlite:///var/lib/kbf/state.db \\\n    --cold-store=file:///var/lib/kbf/cold \\\n    --listen=127.0.0.1:8980\n";

/// The same unit after the crossing: the new link, the host's Postgres, every other byte as it was.
fn aio_after() -> String {
    AIO_UNIT
        .replace(
            "/usr/local/kbf/current",
            "/var/lib/kbf-installer/releases/current",
        )
        .replace("sqlite:///var/lib/kbf/state.db", LOCAL_POSTGRES)
}

const SQLITE: &str = "var/lib/kbf/state.db";

/// A host of the old layout that also runs an all-in-one unit on a SQLite state file. Its
/// scratch directory is named for the test with a `pg-` prefix: the rigs of the other tests of
/// the module have names like `full` and `crash`, and one scratch directory is one host.
fn frontend(name: &str, schema: Option<u32>) -> (Rig, FakeDb) {
    let rig = Rig::linux(&format!("pg-{name}"));
    put(
        &rig.at("etc/systemd/system/kbf-all-in-one.service"),
        AIO_UNIT,
        0o644,
    );
    rig.active
        .borrow_mut()
        .insert("kbf-all-in-one.service".to_owned());
    put(&rig.at(SQLITE), "SQLite format 3\0rows of the farm", 0o600);
    let db = FakeDb::new(&rig, schema);
    (rig, db)
}

/// The migration of a frontend or all-in-one host: both units, and the database.
fn crossing(rig: &Rig, db: &FakeDb) -> Migration {
    let mut m = rig.migration();
    m.units.push(unit("kbf-all-in-one.service"));
    m.state_store = Some(StateStore {
        url: LOCAL_POSTGRES.to_owned(),
        database: Box::new(db.clone()),
    });
    m
}

fn go_with(
    rig: &mut Rig,
    m: &Migration,
    runner: &dyn Runner,
    mode: Mode,
) -> Result<Report, MigrateError> {
    migrate(
        m,
        &mut rig.host,
        &Hooks {
            runner,
            verifier: &never(),
            frontend: &NoFrontend,
        },
        mode,
    )
}

fn go(rig: &mut Rig, m: &Migration, mode: Mode) -> Result<Report, MigrateError> {
    go_with(rig, m, &runner(), mode)
}

fn read(rig: &Rig, path: &str) -> String {
    fs::read_to_string(rig.at(path)).expect("a file")
}

/// Catches the heart of the crossing: a frontend or all-in-one host gets its schema first
/// (while the old frontend still serves), then everything stops, the unit's path and its
/// `--state-store` are written in one pass (the SQLite file becomes the host's Postgres, every
/// other byte of the unit as it was, the comment that names the flag too), and the units start
/// again. The journal says the store was moved, the unit's original is kept for a rollback, the
/// report says what the schema step did and warns that nothing is carried over, and a node unit
/// of the same host (no store flag) is left alone and says so.
///
/// Mutants seen red: the switch skipped (the unit keeps the file); the schema step after the
/// stop (the first event is a stop); the schema step skipped; the warning dropped.
#[test]
fn a_frontend_host_gets_its_schema_then_stops_switches_its_store_and_starts() {
    let (mut rig, db) = frontend("full", None);
    let m = crossing(&rig, &db);
    let report = go(&mut rig, &m, Mode::Apply).expect("migrate");

    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(
        *rig.events.borrow(),
        [
            "database setup",
            "stop kbf-helper.service links=false",
            "stop kbf-daemon.service links=false",
            "stop kbf-all-in-one.service links=false",
            "disable kbf-helper.service links=false",
            "daemon-reload links=true",
            "start kbf-daemon.service links=true",
            "start kbf-all-in-one.service links=true",
        ]
    );
    assert_eq!(db.calls(), ["plan", "setup"]);
    assert_eq!(db.schema(), Some(3));

    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-all-in-one.service"),
        aio_after()
    );
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-daemon.service"),
        OLD_UNIT.replace(
            "/usr/local/kbf/current",
            "/var/lib/kbf-installer/releases/current"
        ),
        "the node unit changes its path and nothing else"
    );
    let tree = rig.new_tree();
    assert_eq!(
        fs::read_to_string(tree.join("migration-units/kbf-all-in-one.service")).expect("backup"),
        AIO_UNIT
    );
    let journal = rig.json(&tree.join("migration.json"));
    assert_eq!(journal["state_store"], json!(LOCAL_POSTGRES));
    assert_eq!(journal["phase"], json!("done"));

    let shown = report.to_string();
    assert!(
        shown.contains("+ state database: created the schema at version 3"),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "kbf-all-in-one.service: --state-store now names {LOCAL_POSTGRES}"
        )),
        "{shown}"
    );
    assert!(
        shown.contains("kbf-daemon.service: gives no --state-store to switch: left alone"),
        "{shown}"
    );
    assert!(report.warnings.iter().any(|w| w == FRESH_START), "{shown}");
    assert!(
        !report.warnings.iter().any(|w| w.contains("no unit gives")),
        "{shown}"
    );
}

/// Catches any copy of the old store: the SQLite file and its siblings are byte-for-byte what
/// they were (content, mode, size, modification time), and the only paths the migration adds
/// are the installer's tree, the two lock files and the note in the old state directory: no file
/// named for the store anywhere, nothing under the SQLite file's directory.
///
/// Mutant seen red: a copy of the SQLite file into the new tree planted after the schema step.
#[test]
fn no_state_is_copied_from_the_sqlite_file() {
    let (mut rig, db) = frontend("nocopy", None);
    // One sibling readable and one not: a copy that reads the store fails on the second (when
    // the tests run unprivileged) or leaves a file for the scan below.
    let files = [
        SQLITE,
        "var/lib/kbf/state.db-wal",
        "var/lib/kbf/state.db-shm",
    ];
    put(&rig.at(files[1]), "write-ahead rows", 0o600);
    put(&rig.at(files[2]), "shared memory", 0o000);
    let stat = |rig: &Rig| {
        files.map(|name| {
            let m = fs::metadata(rig.at(name)).expect("meta");
            (m.len(), m.mode(), m.mtime(), m.mtime_nsec())
        })
    };
    let before = rig.snapshot("");
    let was = stat(&rig);

    let m = crossing(&rig, &db);
    go(&mut rig, &m, Mode::Apply).expect("migrate");

    let after = rig.snapshot("");
    let store = |snap: &BTreeMap<String, String>| {
        snap.iter()
            .filter(|(path, _)| path.starts_with("var/lib/kbf/"))
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(store(&before), store(&after), "the store's directory");
    assert_eq!(was, stat(&rig));
    for (path, _) in after.iter().filter(|(path, _)| !before.contains_key(*path)) {
        assert!(
            path.starts_with("var/lib/kbf-installer")
                || path.starts_with("run/")
                || path == "var/lib/kbf-deploy/MIGRATED",
            "{path} is new"
        );
        assert!(!path.contains("state.db"), "{path} is a copy of the store");
    }
}

/// Every event the kernel has queued on `watch`, as "name flags" lines.
#[cfg(target_os = "linux")]
fn events(watch: &std::os::fd::OwnedFd) -> Vec<String> {
    use rustix::fs::inotify::Reader;
    let mut buffer = [std::mem::MaybeUninit::uninit(); 8192];
    let mut reader = Reader::new(watch, &mut buffer);
    let mut seen = Vec::new();
    loop {
        match reader.next() {
            Ok(event) => seen.push(format!("{:?} {:?}", event.file_name(), event.events())),
            Err(rustix::io::Errno::AGAIN) => return seen,
            Err(e) => panic!("reading inotify events: {e}"),
        }
    }
}

/// Catches a read of the SQLite file that copies nothing: the kernel is asked, from outside the
/// code under test, whether anything opened, read, wrote, closed, created, removed or changed
/// the attributes of the file or its siblings while a dry run and an apply ran. (Time stamps
/// cannot tell: a mount that does not keep access times shows no read, and a read that is
/// thrown away leaves no copy for `no_state_is_copied_from_the_sqlite_file` to find.) The
/// control comes first: a plain read of the file is seen, so that silence afterwards is a
/// migration that did not touch it, and not a file system that reports nothing.
///
/// Mutants seen red: a read of the file (`fs::read`) planted after the schema step, its result
/// dropped; the same on the sibling write-ahead file.
#[cfg(target_os = "linux")]
#[test]
fn the_sqlite_file_is_not_opened_by_a_dry_run_or_an_apply() {
    use rustix::fs::inotify::{self, CreateFlags, WatchFlags};

    let (mut rig, db) = frontend("noread", None);
    put(
        &rig.at("var/lib/kbf/state.db-wal"),
        "write-ahead rows",
        0o600,
    );
    let watch = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK).expect("inotify");
    inotify::add_watch(&watch, rig.at("var/lib/kbf"), WatchFlags::ALL_EVENTS).expect("watch");

    fs::read(rig.at(SQLITE)).expect("the control read");
    let control = events(&watch);
    assert!(
        control
            .iter()
            .any(|e| e.contains("state.db") && e.contains("OPEN")),
        "a read of the file is not seen, so silence would prove nothing: {control:?}"
    );

    let m = crossing(&rig, &db);
    for mode in [Mode::DryRun, Mode::Apply] {
        go(&mut rig, &m, mode).expect("migrate");
        assert_eq!(events(&watch), Vec::<String>::new(), "{mode:?}");
    }
    assert_eq!(db.calls(), ["plan", "plan", "setup"]);
}

/// What the build of `commit` says to `--version --json`, as the rig's other builds say it
/// (a release build of that commit) except for its state schema: 0, a build that embeds no
/// migration and so cannot open a Postgres store.
fn schema_less(commit: &'static str) -> FakeRunner {
    FakeRunner::new(move |spawn| {
        let dir = spawn
            .program
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .expect("a release directory");
        let says = version_json(dir, "release");
        Ok(ran(
            0,
            &if dir == commit {
                says.replace("\"state_schema\":7", "\"state_schema\":0")
            } else {
                says
            },
            "",
        ))
    })
}

/// Catches a crossing that stops the farm onto a release that cannot open its store: the units
/// run `current`, and a `current` that reports no state schema (it embeds no Postgres
/// migration) is refused with exit 3 before the schema step, any stop or any file, in a dry run
/// too. The database was asked (the refusal of an absent Postgres comes first); it was never set
/// up.
///
/// Mutants seen red: the check skipped; the check on `previous` in place of `current`.
#[test]
fn a_current_release_that_embeds_no_postgres_schema_refuses_before_anything_changes() {
    let (mut rig, db) = frontend("oldcur", None);
    let m = crossing(&rig, &db);
    let before = rig.snapshot("");
    for mode in [Mode::DryRun, Mode::Apply] {
        let error = go_with(&mut rig, &m, &schema_less(A), mode).expect_err("refused");
        assert!(
            matches!(&error, MigrateError::NoPostgresSchema { release } if release == A),
            "{error}"
        );
        assert_eq!(error.exit().code(), 3);
        let said = error.to_string();
        assert!(
            said.contains(&A[..12]) && said.contains("state_schema 0"),
            "{said}"
        );
    }
    assert_eq!(db.calls(), ["plan", "plan"], "asked twice, never set up");
    assert_eq!(db.schema(), None);
    assert_eq!(rig.snapshot(""), before);
    assert!(rig.events.borrow().is_empty() && rig.host.log.is_empty());
}

/// Catches a check that is too wide or too narrow: a `previous` that embeds no schema is
/// adopted and warned of (a revert to it could not open the store), in a dry run and an
/// apply, and the crossing goes on; a node host (no state store) is not judged by the schema
/// at all, so its `current` with none migrates and says nothing of it.
///
/// Mutants seen red: the `previous` refused like `current`; the warning dropped; the node host
/// judged.
#[test]
fn a_previous_release_with_no_postgres_schema_is_warned_of_and_a_node_is_not_judged() {
    let (mut rig, db) = frontend("oldprev", None);
    let m = crossing(&rig, &db);
    let warned = |report: &Report| {
        report
            .warnings
            .iter()
            .filter(|w| w.contains(&B[..12]) && w.contains("revert"))
            .count()
    };
    let planned = go_with(&mut rig, &m, &schema_less(B), Mode::DryRun).expect("plan");
    assert_eq!(warned(&planned), 1, "{planned}");
    assert!(db.calls() == ["plan"] && rig.events.borrow().is_empty());
    let report = go_with(&mut rig, &m, &schema_less(B), Mode::Apply).expect("migrate");
    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(warned(&report), 1, "{report}");
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-all-in-one.service"),
        aio_after()
    );

    let mut node = Rig::linux("pg-oldnode");
    let plain = node.migration();
    assert!(plain.state_store.is_none());
    let planned = go_with(&mut node, &plain, &schema_less(A), Mode::DryRun).expect("plan");
    assert!(planned.warnings.iter().all(|w| !w.contains("state schema")));
    let report = go_with(&mut node, &plain, &schema_less(A), Mode::Apply).expect("migrate");
    assert_eq!(report.outcome, Outcome::Migrated);
    assert!(report.warnings.iter().all(|w| !w.contains("state schema")));
}

/// Catches a Postgres step on a node: a migration with no state store (a storage, daemon or
/// storage-and-daemon host, or one whose mode was not said) asks no database, writes no schema
/// event, prints no database line, journals no store, and leaves even a `--state-store=sqlite`
/// on a command line as it is; its rollback says nothing of a store. The same in a dry run.
///
/// Mutant seen red: the rollback's store line printed whatever the journal says. (That a node
/// mode gets no state store at all is the app test `only_the_frontend_and_all_in_one_modes...`.)
#[test]
fn a_node_host_has_no_database_step() {
    let mut rig = Rig::linux("pg-node");
    let storage = "[Service]\nExecStart=/usr/local/kbf/current/kbf storage \\\n    --state-store=sqlite:///var/lib/kbf/state.db\n";
    put(
        &rig.at("etc/systemd/system/kbf-storage.service"),
        storage,
        0o644,
    );
    let mut m = rig.migration();
    m.units.push(unit("kbf-storage.service"));
    assert!(m.state_store.is_none());

    let planned = go(&mut rig, &m, Mode::DryRun).expect("plan");
    let shown = planned.to_string();
    assert!(
        !shown.contains("state database") && !shown.contains("--state-store"),
        "{shown}"
    );
    assert!(planned.warnings.iter().all(|w| w != FRESH_START));

    let report = go(&mut rig, &m, Mode::Apply).expect("migrate");
    let shown = report.to_string();
    assert!(!shown.contains("state database"), "{shown}");
    assert!(report.warnings.iter().all(|w| w != FRESH_START));
    assert!(
        rig.events.borrow().iter().all(|e| !e.contains("database")),
        "{:?}",
        rig.events.borrow()
    );
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-storage.service"),
        storage.replace(
            "/usr/local/kbf/current",
            "/var/lib/kbf-installer/releases/current"
        ),
        "the flag stays: no state store was asked to move"
    );
    let journal = rig.json(&rig.new_tree().join("migration.json"));
    assert_eq!(journal["state_store"], json!(null));

    let back = rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    assert!(!back.to_string().contains("SQLite"), "{back}");
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-storage.service"),
        storage
    );
}

/// Catches a migration that creates what it was told to expect: a Postgres that cannot be
/// asked (absent, or without the `kbf` role or database) refuses a dry run and an apply with
/// exit 2 naming what must exist, before any program runs, any service is touched or any file
/// is written, and the schema step is never run.
///
/// Mutants seen red: the refusal swallowed (the dry run went on to plan); the database asked
/// after the version probe (the runner was called); the refusal ending with exit 1.
#[test]
fn a_missing_postgres_refuses_before_anything_changes() {
    let (mut rig, db) = frontend("absent", None);
    db.db.borrow_mut().down = Some("connection refused".to_owned());
    let m = crossing(&rig, &db);
    let before = rig.snapshot("");
    for mode in [Mode::DryRun, Mode::Apply] {
        let probe = runner();
        let error = go_with(&mut rig, &m, &probe, mode).expect_err("refused");
        assert!(
            matches!(&error, MigrateError::NoStateDatabase(why) if why == "connection refused"),
            "{error}"
        );
        assert_eq!(error.exit().code(), 2);
        let said = error.to_string();
        assert!(
            said.contains("`kbf` role") && said.contains("creates none of them"),
            "{said}"
        );
        // The URL names no role, so the server is asked as the user running the verb (root,
        // for an apply): a host with a correct `kbf` role is refused too when it lets only
        // `kbf` in, and the line must say whose connection it is.
        assert!(
            said.contains("the user running this verb"),
            "the line names whose connection is refused: {said}"
        );
        assert!(probe.calls().is_empty(), "no program ran");
    }
    assert_eq!(db.calls(), ["plan", "plan"], "asked twice, never set up");
    assert_eq!(db.schema(), None);
    assert_eq!(rig.snapshot(""), before);
    assert!(rig.events.borrow().is_empty() && rig.host.log.is_empty());
}

/// Catches a refused schema step that leaves a stopped host: when the step is refused after the
/// database answered (a contract step pending, a server that will not migrate) the run ends
/// with exit 1 having stopped nothing, written nothing and started nothing.
///
/// Mutants seen red: the schema step moved after the stop (the units were stopped, the tree
/// existed); the schema step skipped (the run migrated).
#[test]
fn a_refused_schema_step_leaves_the_host_as_it_was() {
    let (mut rig, db) = frontend("refused", Some(1));
    db.db.borrow_mut().refuses = Some("migration 2 is a contract step".to_owned());
    let m = crossing(&rig, &db);
    // The two lock files under `run/` are made before the step; nothing else may be.
    let quiet = |snap: BTreeMap<String, String>| {
        snap.into_iter()
            .filter(|(path, _)| !path.starts_with("run/"))
            .collect::<Vec<_>>()
    };
    let before = quiet(rig.snapshot(""));
    let error = go(&mut rig, &m, Mode::Apply).expect_err("refused");
    assert!(
        matches!(&error, MigrateError::StateSchema(why) if why.contains("contract step")),
        "{error}"
    );
    assert_eq!(error.exit().code(), 1);
    assert_eq!(db.calls(), ["plan", "setup"]);
    assert_eq!(quiet(rig.snapshot("")), before);
    assert!(rig.events.borrow().is_empty() && rig.host.log.is_empty());
    assert!(rig.active.borrow().contains("kbf-all-in-one.service"));
}

/// Catches a dry run that changes anything or hides a step: for each state the database can be
/// in it asks (never sets up), names the database and what the schema step would do, names the
/// units whose store would move and says nothing is copied, carries the warning, and leaves the
/// host and the fake as they were; a database ahead of this installer is refused (exit 1)
/// rather than planned.
///
/// Mutants seen red: the dry run calling the schema step; the plan to bring a schema forward
/// taking the case of a schema that is current (`<=` for `<`).
#[test]
fn the_dry_run_shows_the_database_steps_and_changes_nothing() {
    for (schema, plan) in [
        (
            None,
            "no schema yet: the schema step creates it at version 3",
        ),
        (
            Some(1),
            "the schema is at version 1: the schema step brings it to 3",
        ),
        (Some(3), "the schema is at version 3: nothing to apply"),
    ] {
        let (mut rig, db) = frontend("dry", schema);
        let m = crossing(&rig, &db);
        let before = rig.snapshot("");
        let report = go(&mut rig, &m, Mode::DryRun).expect("plan");
        let shown = report.to_string();
        assert_eq!(report.outcome, Outcome::Planned);
        assert!(
            shown.contains(&format!(
                "DRY-RUN: state database ({LOCAL_POSTGRES}): {plan}"
            )),
            "{shown}"
        );
        assert!(
            shown.contains(&format!(
                "DRY-RUN: --state-store=sqlite://... of the --unit units \
                 [kbf-daemon.service, kbf-all-in-one.service] becomes {LOCAL_POSTGRES}: \
                 the SQLite file is left as it is and no state is copied"
            )),
            "{shown}"
        );
        assert!(report.warnings.iter().any(|w| w == FRESH_START));
        assert_eq!(db.calls(), ["plan"]);
        assert_eq!(db.schema(), schema);
        assert_eq!(rig.snapshot(""), before);
        assert!(rig.events.borrow().is_empty() && rig.host.log.is_empty());
    }

    let (mut rig, db) = frontend("ahead", Some(5));
    let m = crossing(&rig, &db);
    let error = go(&mut rig, &m, Mode::DryRun).expect_err("refused");
    assert!(
        matches!(&error, MigrateError::StateSchema(why) if why.contains("past the 3")),
        "{error}"
    );
    assert_eq!(error.exit().code(), 1);
    assert_eq!(db.calls(), ["plan"]);
}

/// Catches a rollback that leaves the host on Postgres or reaches for the database: the units
/// get their originals back (the SQLite store in the all-in-one unit), the SQLite file is as
/// it was, the database is not asked and not dropped, the dry run and the run say the units
/// are back on the file and warn that what was written to Postgres is not copied back, and a
/// migrate after it crosses again (the schema step finds its work done).
///
/// Mutants seen red: the journal not recording the store (the rollback says nothing of it); the
/// rollback's store line inverted; the switch skipped (the unit after the second migrate).
#[test]
fn a_rollback_puts_the_sqlite_store_back_and_copies_nothing_back() {
    let (mut rig, db) = frontend("rollback", None);
    let m = crossing(&rig, &db);
    go(&mut rig, &m, Mode::Apply).expect("migrate");
    let asked = db.calls();
    let sqlite = read(&rig, SQLITE);

    let planned = rollback(&m, &mut rig.host, Mode::DryRun).expect("plan");
    let shown = planned.to_string();
    assert!(
        shown.contains("DRY-RUN: the units' --state-store names the SQLite file again"),
        "{shown}"
    );
    assert!(shown.contains("not copied back"), "{shown}");

    let report = rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    let shown = report.to_string();
    assert!(
        shown.contains("+ the units' --state-store names the SQLite file again"),
        "{shown}"
    );
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-all-in-one.service"),
        AIO_UNIT
    );
    assert_eq!(read(&rig, SQLITE), sqlite);
    assert_eq!(db.calls(), asked, "a rollback asks the database nothing");
    assert_eq!(db.schema(), Some(3), "and drops nothing");

    let again = go(&mut rig, &m, Mode::Apply).expect("migrate again");
    let shown = again.to_string();
    assert!(
        shown.contains("state database: the schema is at version 3, nothing to apply"),
        "{shown}"
    );
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-all-in-one.service"),
        aio_after()
    );
}

/// Catches steps that cannot be repeated: a run that dies at the start (the schema made, the
/// units switched, nothing running) is finished by the next, which finds the schema done
/// (nothing to apply), the unit already on Postgres (reported, bytes unchanged, the original
/// still the first one kept), a unit with no store left alone, and a unit that is not there
/// skipped; and everything that was stopped starts.
///
/// Mutants seen red: the schema step skipped; the switch skipped.
#[test]
fn a_run_that_died_mid_steps_is_finished_by_the_next() {
    let (mut rig, db) = frontend("crash", None);
    let mut m = crossing(&rig, &db);
    m.units.push(unit("absent.service"));
    rig.fail_start.set(true);
    let error = go(&mut rig, &m, Mode::Apply).expect_err("start fails");
    assert!(
        error.to_string().contains("planted: start failed"),
        "{error}"
    );
    assert!(rig.active.borrow().is_empty(), "everything stopped");
    assert_eq!(db.schema(), Some(3));
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-all-in-one.service"),
        aio_after()
    );
    let tree = rig.new_tree();
    let journal = rig.json(&tree.join("migration.json"));
    assert_eq!(journal["phase"], json!("started"));
    assert_eq!(journal["state_store"], json!(LOCAL_POSTGRES));

    rig.fail_start.set(false);
    let report = go(&mut rig, &m, Mode::Apply).expect("finish");
    let shown = report.to_string();
    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(db.calls(), ["plan", "setup", "plan", "setup"]);
    assert!(
        shown.contains("state database: the schema is at version 3, nothing to apply"),
        "{shown}"
    );
    for line in [
        "kbf-all-in-one.service: --state-store already names a Postgres store",
        "kbf-daemon.service: gives no --state-store to switch: left alone",
        "absent.service: no such unit file: skipped",
    ] {
        assert!(shown.contains(line), "{line}: {shown}");
    }
    assert_eq!(
        read(&rig, "etc/systemd/system/kbf-all-in-one.service"),
        aio_after()
    );
    assert_eq!(
        fs::read_to_string(tree.join("migration-units/kbf-all-in-one.service")).expect("backup"),
        AIO_UNIT,
        "the first original is still the one kept"
    );
    let active = rig.active.borrow();
    assert!(active.contains("kbf-daemon.service") && active.contains("kbf-all-in-one.service"));
    assert!(!active.contains("kbf-helper.service"));
    assert_eq!(
        rig.json(&tree.join("migration.json"))["phase"],
        json!("done")
    );
}

/// Catches a switch that reports success where nothing could be switched: a frontend host
/// whose units give no `--state-store` (the store is then kbf.toml's or the mode's default) has
/// its schema made and is told, in a warning, that no unit was moved.
///
/// Mutant seen red: the warning's condition inverted.
#[test]
fn a_host_whose_units_name_no_store_is_told_none_was_switched() {
    let (mut rig, db) = frontend("nostore", None);
    let mut m = crossing(&rig, &db);
    m.units = vec![rig.daemon()];
    let report = go(&mut rig, &m, Mode::Apply).expect("migrate");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("no unit gives --state-store")),
        "{report}"
    );
    assert_eq!(db.schema(), Some(3));
}

/// Catches a journal that cannot be read after the field was added, and the wrong codes:
/// a `migration.json` written before `state_store` existed reads as no store moved, and the
/// two new refusals end with the exits the contract names (2, a prerequisite; 1, tried and
/// failed).
///
/// Mutant seen red: the missing-database refusal ending with exit 1.
#[test]
fn an_old_journal_reads_as_no_store_moved_and_the_refusals_have_their_exits() {
    let old =
        br#"{"schema":1,"phase":"done","units":[],"helper_unit":null,"stopped":[],"adopted":[]}"#;
    let journal = journal::Journal::parse(old).expect("an old journal");
    assert_eq!(journal.state_store, None);

    assert_eq!(
        MigrateError::NoStateDatabase(String::new()).exit().code(),
        2
    );
    assert_eq!(MigrateError::StateSchema(String::new()).exit().code(), 1);
}

/// Catches the real database being wired wrong: the placement is a socket URL without a
/// password that the parser accepts and the store carries; asking a database nobody serves
/// (the socket's directory is not there) is an error line from the installer's own step, for
/// the plan and for the schema step alike; and the report line of a step that ran is the
/// step's outcome as it prints.
///
/// No mutant is planted here: these arms run only where no server answers, which is what the
/// coverage ratchet needs of them.
#[test]
fn the_real_database_is_asked_through_the_installers_schema_step() {
    let store = StateStore::local();
    assert_eq!(store.url, LOCAL_POSTGRES);
    let config = PostgresConfig::parse(LOCAL_POSTGRES).expect("a socket url");
    assert!(!config.has_password());

    let nobody = StateStore::at("postgres:///kbf?host=/nonexistent-dir-for-kbf-tests");
    assert_eq!(
        nobody.url,
        "postgres:///kbf?host=/nonexistent-dir-for-kbf-tests"
    );
    let plan = nobody.database.plan().expect_err("nobody serves it");
    let setup = nobody.database.setup().expect_err("nobody serves it");
    assert!(!plan.is_empty() && !setup.is_empty());
    // A URL the parser refuses is a line of the step too, not a panic.
    let bad = StateStore::at("not a postgres url");
    assert!(bad.database.plan().is_err() && bad.database.setup().is_err());

    assert_eq!(
        applied(Ok(SchemaOutcome {
            from: None,
            to: 3,
            ran: vec![1, 2, 3]
        })),
        Ok("created the schema at version 3".to_owned())
    );
    let refused = applied(Err(SchemaError::NotInitialised)).expect_err("an error line");
    assert!(refused.contains("no schema"), "{refused}");
}
