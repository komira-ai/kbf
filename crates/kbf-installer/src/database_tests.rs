//! `installer.toml`'s database setting (no server), and `setup` and the update step against
//! a real server (`KBF_TEST_POSTGRES_URL`, tests `#[ignore]`d like the state crate's).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_state::postgres::schema::{self, Migration, Outcome, SchemaError};
use kbf_state::postgres::{PostgresConfig, PostgresStore};
use kbf_state::{Cond, Store, Txn};

use super::{
    Database, DatabaseError, SchemaOps, SchemaStep, apply_line, database_step, dry_run_line,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn toml_file(body: &str) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = kbf_host::testkit::scratch(&format!("database-{n}"));
    let file = dir.join("installer.toml");
    fs::write(&file, body).expect("written");
    file
}

/// Catches an installer.toml read wrongly: the one key is found among keys other readers
/// own, and every way it can be wrong is a refusal that does not quote the URL.
///
/// Mutants seen red: the password check removed (`Password` never returned); the error
/// for a bad URL formatted with the URL.
#[test]
fn the_database_setting_is_read_and_every_fault_is_refused_without_quoting_the_url() {
    let ok = toml_file(
        "release_source = \"url\"\ndatabase = \"postgres:///kbf?host=/run/postgresql\"\n",
    );
    assert!(Database::from_installer_toml(&ok).is_ok());

    let missing = kbf_host::testkit::scratch("database-none").join("no-such-installer.toml");
    assert!(matches!(
        Database::from_installer_toml(&missing),
        Err(DatabaseError::Unreadable { .. })
    ));
    assert!(matches!(
        Database::from_installer_toml(&toml_file("database = = =")),
        Err(DatabaseError::NotToml(_))
    ));
    for body in ["x = 1", "database = 5"] {
        assert!(matches!(
            Database::from_installer_toml(&toml_file(body)),
            Err(DatabaseError::NoDatabase(_))
        ));
    }
    for (url, secret) in [
        ("postgres://kbf:hunter2@db/kbf", "hunter2"),
        (
            "postgres:///kbf?host=/run/postgresql&password=hunter2",
            "hunter2",
        ),
    ] {
        let err = Database::from_installer_toml(&toml_file(&format!("database = \"{url}\"")))
            .unwrap_err();
        assert!(matches!(err, DatabaseError::Password(_)), "{url}");
        assert!(
            !err.to_string().contains(secret) && !err.to_string().contains("postgres"),
            "{err}"
        );
    }
    let tls = "postgres://db.example/kbf?sslmode=require";
    let err =
        Database::from_installer_toml(&toml_file(&format!("database = \"{tls}\""))).unwrap_err();
    assert!(matches!(err, DatabaseError::BadUrl { .. }));
    assert!(!err.to_string().contains("db.example"), "{err}");
}

// ---- against a server -----------------------------------------------------------------

fn url() -> String {
    std::env::var("KBF_TEST_POSTGRES_URL").expect("KBF_TEST_POSTGRES_URL names a test database")
}

struct Scratch(String);

impl Scratch {
    fn new() -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Scratch(format!("kbf_i{}_{n}", std::process::id()))
    }

    fn config(&self) -> PostgresConfig {
        PostgresConfig::parse(&url())
            .and_then(|c| c.with_schema(&self.0))
            .expect("parses")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(mut c) = postgres::Client::connect(&url(), postgres::NoTls) {
            let _ = c.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.0));
        }
    }
}

fn next_release() -> Vec<Migration> {
    let mut set = schema::embedded();
    set.push(
        Migration::new(
            2,
            "note",
            "-- kbf-migration: expand\nALTER TABLE kv ADD COLUMN note text;",
        )
        .expect("lints"),
    );
    set
}

fn put(store: &PostgresStore, key: &str) {
    store
        .txn(Txn::new().put(key, "v", Cond::Any))
        .expect("reachable")
        .expect("commits");
}

/// Catches `setup` leaving a database kbf refuses, and a dry run that writes. Dry run on a
/// fresh database reports and creates nothing; `setup` creates the schema; a store opens
/// on it; `setup` again changes nothing.
///
/// Mutant seen red: `plan` made to call `setup` (the dry run created the schema).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn setup_creates_the_schema_once_and_a_dry_run_creates_nothing() {
    let s = Scratch::new();
    let db = Database::new(s.config(), schema::embedded());
    assert_eq!(db.plan(), Ok((None, 1)));
    assert_eq!(
        schema::current(&s.config()),
        Err(SchemaError::NotInitialised)
    );
    let out = db.setup().expect("setup");
    assert_eq!((out.from, out.to), (None, 1));
    assert_eq!(db.plan(), Ok((Some(1), 1)));
    PostgresStore::open(&s.config()).expect("kbf starts on what setup made");
    assert!(db.setup().expect("again").ran.is_empty());
}

/// Catches the update step migrating to the wrong version or breaking the serving child.
/// Child N is serving on schema 1; the update step for a release that needs schema 2 runs
/// the new migration; child N keeps writing; a release that needs 3 (an installer older
/// than the release) is refused and the schema stays at 2; a release that needs 1 changes
/// nothing.
///
/// Mutant seen red: the step applying the installer's newest instead of the release's
/// (the schema moved past what the release asked for).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn the_update_step_migrates_to_what_the_release_needs_while_the_old_child_serves() {
    let s = Scratch::new();
    Database::new(s.config(), schema::embedded())
        .setup()
        .expect("N installed");
    let child = PostgresStore::open(&s.config().with_supported_schema(1, 1)).expect("child N");
    put(&child, "a");

    let mut db = Database::new(s.config(), next_release());
    assert_eq!(
        db.before_start(1).expect("nothing to do"),
        "the schema is at version 1, nothing to apply"
    );
    assert_eq!(schema::current(&s.config()), Ok(1));
    assert_eq!(
        db.before_start(2).expect("migrates"),
        "migrated the schema from 1 to 2"
    );
    put(&child, "b");
    assert_eq!(schema::current(&s.config()), Ok(2));

    let err = db.before_start(3).unwrap_err();
    assert!(
        err.contains("needs state schema 3") && err.contains("update the kbf-installer package"),
        "{err}"
    );
    assert!(!err.contains('\n'));
    assert_eq!(schema::current(&s.config()), Ok(2));
}

/// Catches the plan and the update-step line mishandling what the server said, without a
/// server: no schema yet is a plan (`None`), any other fault stays an error, and the step's
/// line is the outcome's text or the error's one line.
#[test]
fn the_plan_and_the_step_line_follow_what_the_server_said() {
    use super::{plan_of, step_line};
    assert_eq!(plan_of(Ok(2), 3).unwrap(), (Some(2), 3));
    assert_eq!(
        plan_of(Err(SchemaError::NotInitialised), 3).unwrap(),
        (None, 3)
    );
    assert!(plan_of(Err(SchemaError::Database("down".into())), 3).is_err());
    let done = schema::Outcome {
        from: Some(1),
        to: 3,
        ran: vec![2, 3],
    };
    assert_eq!(
        step_line(Ok(done)).unwrap(),
        "migrated the schema from 1 to 3"
    );
    assert_eq!(
        step_line(Err(SchemaError::NotInitialised)).unwrap_err(),
        SchemaError::NotInitialised.to_string()
    );
}

/// Catches the step against an unreachable server (the socket directory is missing) hiding
/// the failure: `setup`, `plan` and the update step each return the database's reason.
#[test]
fn setup_plan_and_the_update_step_report_an_unreachable_server() {
    let file = toml_file("database = \"postgres:///kbf?host=/nonexistent-kbf-test-socket\"\n");
    let mut db = Database::from_installer_toml(&file).expect("a usable url");
    assert!(db.setup().is_err());
    assert!(db.plan().is_err());
    assert!(!db.before_start(1).unwrap_err().is_empty());
}

/// Catches the database step's three dry-run lines and its apply line going wrong: a fresh
/// database would be created, an older one migrated, a current one left alone, and an apply
/// says what the schema runner did (the lines need no server, so they are tested here).
///
/// Mutants to plant: the `v < newest` guard flipped (a behind schema says nothing to do).
#[test]
fn the_database_step_lines_say_create_migrate_or_nothing() {
    assert_eq!(
        dry_run_line("frontend", None, 3),
        "kbf-installer setup (frontend): database step: dry run: would create the schema at version 3"
    );
    assert_eq!(
        dry_run_line("frontend", Some(1), 3),
        "kbf-installer setup (frontend): database step: dry run: would migrate the schema from 1 to 3"
    );
    assert_eq!(
        dry_run_line("all-in-one", Some(3), 3),
        "kbf-installer setup (all-in-one): database step: dry run: the schema is at 3, nothing to do"
    );
    let outcome = Outcome {
        from: None,
        to: 2,
        ran: vec![1, 2],
    };
    assert_eq!(
        apply_line("frontend", &outcome),
        "kbf-installer setup (frontend): database step: created the schema at version 2"
    );
}

/// A schema runner that answers as told, standing in for a server.
struct FakeSchema {
    setup: Result<Outcome, String>,
    plan: Result<(Option<u32>, u32), String>,
}

impl SchemaOps for FakeSchema {
    fn setup(&self) -> Result<Outcome, String> {
        self.setup.clone()
    }

    fn plan(&self) -> Result<(Option<u32>, u32), String> {
        self.plan.clone()
    }
}

/// Catches the database step printing the wrong line once the schema runner answers, or
/// swallowing its error: `--apply` prints the outcome, a dry run prints create, migrate or
/// nothing to do, and a runner's error is the failure. The fake answers an error to
/// the question the step must not ask, so asking it fails the test.
///
/// Mutants to plant: `apply` flipped; the error mapped to an empty line.
#[test]
fn the_database_step_prints_what_the_schema_runner_answered() {
    let outcome = Outcome {
        from: None,
        to: 2,
        ran: vec![1, 2],
    };
    let fake = |plan| FakeSchema {
        setup: Ok(outcome.clone()),
        plan,
    };
    assert_eq!(
        database_step(&fake(Err("unused".into())), "frontend", true)
            .unwrap_or_else(|f| panic!("applied: {f}")),
        "kbf-installer setup (frontend): database step: created the schema at version 2"
    );
    for (found, tail) in [
        (None, "would create the schema at version 3"),
        (Some(1), "would migrate the schema from 1 to 3"),
        (Some(3), "the schema is at 3, nothing to do"),
    ] {
        assert_eq!(
            database_step(&fake(Ok((found, 3))), "all-in-one", false)
                .unwrap_or_else(|f| panic!("planned: {f}")),
            format!("kbf-installer setup (all-in-one): database step: dry run: {tail}")
        );
    }
    let broken = FakeSchema {
        setup: Err("no server".into()),
        plan: Err("cannot ask".into()),
    };
    let apply = database_step(&broken, "frontend", true).expect_err("setup failed");
    assert_eq!(apply, "no server");
    let plan = database_step(&broken, "frontend", false).expect_err("plan failed");
    assert_eq!(plan, "cannot ask");
}

/// Catches the real `Database` not being what `setup`'s database step asks (the fake-runner
/// tests drive `database_step` alone): through `SchemaOps`, with a socket directory that does
/// not exist (so no server is needed), both the plan and the setup reach the database and
/// answer with its own reason, never the URL.
///
/// Mutants to plant: `Database::plan` or `Database::setup` swapped in `impl SchemaOps`.
#[test]
fn the_real_database_answers_the_schema_ops_with_its_own_error() {
    let url = "postgres:///kbf?host=/nonexistent-kbf-test-socket";
    let db = Database::new(
        PostgresConfig::parse(url).expect("a url"),
        schema::embedded(),
    );
    let planned = SchemaOps::plan(&db).expect_err("no server to ask");
    let applied = SchemaOps::setup(&db).expect_err("no server to apply to");
    for why in [planned, applied] {
        assert!(
            !why.is_empty() && !why.contains("nonexistent-kbf-test-socket"),
            "{why}"
        );
    }
}
