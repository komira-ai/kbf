//! The schema the installer owns, against a real server: creating it, migrating it under
//! a serving store, and the refusals. Each test makes a database of its own and drops it, so
//! the schema in it (the default one, `kbf_state`) and the schema lock, which is one per
//! database, are the test's alone.
//!
//! The tests need a server and fail without one, so the pull-request gate runs them with the
//! rest. `KBF_TEST_POSTGRES_URL` names a server they may create and drop databases on.
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use kbf_state::postgres::schema::{self, Migration, SchemaError};
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase};
use kbf_state::{Cond, Store, StoreError, Txn};
use postgres::{Client, NoTls};

/// The schema the store and the installer use, which each test's database holds alone.
const SCHEMA: &str = PostgresConfig::DEFAULT_SCHEMA;

/// A database no other test uses, dropped with the guard.
struct Scratch(TestDatabase);

impl Scratch {
    fn new() -> Scratch {
        Scratch(TestDatabase::empty())
    }

    fn url(&self) -> String {
        self.0.url()
    }

    fn config(&self) -> PostgresConfig {
        PostgresConfig::parse(&self.url()).expect("parses")
    }

    /// `statement`, run with the schema first on the search path.
    fn sql(&self, statement: &str) -> Result<(), postgres::Error> {
        Client::connect(&self.url(), NoTls)?
            .batch_execute(&format!("SET search_path TO {SCHEMA}; {statement}"))
    }

    fn query(&self, statement: &str) -> Vec<postgres::Row> {
        let mut client = Client::connect(&self.url(), NoTls).expect("connects");
        client
            .batch_execute(&format!("SET search_path TO {SCHEMA}"))
            .ok();
        client.query(statement, &[]).expect("the query runs")
    }

    fn has_schema(&self) -> bool {
        let mut client = Client::connect(&self.url(), NoTls).expect("connects");
        !client
            .query(
                "SELECT 1 FROM pg_namespace WHERE nspname = $1",
                &[&PostgresConfig::DEFAULT_SCHEMA],
            )
            .expect("catalog")
            .is_empty()
    }
}

/// Release N+1's migration, N being the newest this build embeds: only adds a column and a
/// table.
fn second() -> Migration {
    Migration::new(
        schema::latest() + 1,
        "note",
        "-- kbf-migration: expand\n\
         ALTER TABLE kv ADD COLUMN note text;\n\
         CREATE TABLE audit (id bigint PRIMARY KEY, at timestamptz NOT NULL DEFAULT now());",
    )
    .expect("an additive migration lints")
}

fn two() -> Vec<Migration> {
    let mut set = schema::embedded();
    set.push(second());
    set
}

fn put(store: &PostgresStore, key: &str) -> Result<(), StoreError> {
    store
        .txn(Txn::new().put(key, "v", Cond::Any))?
        .expect("an unconditional put commits");
    Ok(())
}

fn refusal(config: &PostgresConfig) -> String {
    match PostgresStore::open(config) {
        Err(StoreError::Failed(why)) => why,
        Err(other) => panic!("not a failure: {other:?}"),
        Ok(_) => panic!("the store opened"),
    }
}

// ---- fresh create ---------------------------------------------------------------------

/// Catches a setup that leaves a database kbf cannot use: after `apply` on an empty
/// database the version row is there, every table the store needs exists, a store opens on
/// it and writes, and a second `apply` runs nothing.
///
/// Catches: the schema_version INSERT left out (`current` says not initialised);
/// a table of migration 1 misnamed (the store's first statement fails).
#[test]
fn a_fresh_database_gets_the_schema_and_a_store_runs_on_it() {
    let s = Scratch::new();
    let config = s.config();
    let latest = schema::latest();
    let versions: Vec<u32> = (1..=latest).collect();
    let out = schema::apply(&config, &schema::embedded(), latest).expect("creates");
    assert_eq!((out.from, out.to), (None, latest));
    assert_eq!(out.ran, versions);
    let said = format!("created the schema at version {latest}");
    assert_eq!(out.to_string(), said);
    assert_eq!(schema::current(&config), Ok(latest));
    let rows = s.query("SELECT version, name, kind FROM schema_version");
    assert_eq!(rows.len(), versions.len());
    for row in &rows {
        assert_eq!(row.get::<_, String>(2), "expand");
    }
    for table in ["kv", "feed", "counters", "head"] {
        s.query(&format!("SELECT count(*) FROM {table}"));
    }
    let store = PostgresStore::open(&config).expect("a store opens on the new schema");
    put(&store, "k").expect("writes");
    assert!(store.get("k").expect("reads").is_some());
    let again = schema::apply(&config, &schema::embedded(), latest).expect("again");
    assert_eq!((again.from, again.to), (Some(latest), latest));
    assert!(again.ran.is_empty());
}

/// Catches kbf creating tables itself: a store opened on a database with no schema must
/// refuse with the one line that says what to run, and leave the database as it found it.
///
/// Catches: `connect` made to run the old `CREATE SCHEMA IF NOT EXISTS` statements
/// (the open succeeded and the schema existed afterwards).
#[test]
fn a_store_does_not_create_the_schema_and_says_what_to_run() {
    let s = Scratch::new();
    let why = refusal(&s.config());
    assert!(
        why.contains("no schema") && why.contains("kbf-installer setup"),
        "{why}"
    );
    assert!(!why.contains('\n'), "one line: {why:?}");
    assert!(!s.has_schema(), "the refused open created a schema");
}

// ---- N to N+1 -------------------------------------------------------------------------

/// Catches an update whose migration breaks the child that is still serving. Child N
/// (supported 1..=N, N the newest this build embeds) has open connections and a writer
/// thread running while the installer applies migration N+1 (an `ALTER TABLE ... ADD
/// COLUMN` and a new table); no write may fail, new connections the pool opens afterwards
/// must work, and a restart of child N on the migrated schema must start (the revert case).
/// Child N+1 (1..=N+1) starts too.
///
/// Catches: the expand rule removed from `check_conn` (the restart refuses). A
/// check on every new connection would not be caught here: with the expand rule it passes
/// too, so only the first connection is judged, to save a query per connect.
#[test]
fn migrating_to_n_plus_1_leaves_the_serving_n_child_working() {
    let s = Scratch::new();
    let installer = s.config();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&installer, &schema::embedded(), n).expect("N is installed");
    let child_n = PostgresStore::open(&installer.clone().with_supported_schema(1, n))
        .expect("child N starts on schema N");
    put(&child_n, "before").expect("writes");

    let stop = AtomicBool::new(false);
    let (written, failed) = thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let (mut ok, mut bad) = (0_u32, 0_u32);
            while !stop.load(Ordering::SeqCst) {
                match put(&child_n, &format!("w{ok}")) {
                    Ok(()) => ok += 1,
                    Err(_) => bad += 1,
                }
            }
            (ok, bad)
        });
        let out = schema::apply(&installer, &two(), n1).expect("N+1 migrates");
        assert_eq!((out.from, out.to), (Some(n), n1));
        assert_eq!(out.ran, vec![n1]);
        // Parallel callers make the pool open connections after the migration.
        thread::scope(|inner| {
            for t in 0..4 {
                let child = &child_n;
                inner.spawn(move || {
                    for i in 0..20 {
                        put(child, &format!("after{t}_{i}")).expect("a write after the migration");
                    }
                });
            }
        });
        stop.store(true, Ordering::SeqCst);
        writer.join().expect("writer")
    });
    assert!(written > 0, "the writer never ran during the migration");
    assert_eq!(
        failed, 0,
        "{failed} writes failed while the schema was migrated"
    );
    assert_eq!(schema::current(&installer), Ok(n1));
    assert!(child_n.get("before").expect("reads").is_some());

    let restarted = child_n
        .reopen()
        .expect("child N restarts on schema N+1: only expand rows are above it");
    put(&restarted, "restarted").expect("writes");
    PostgresStore::open(&installer.with_supported_schema(1, n1)).expect("child N+1 starts");
}

// ---- refusals -------------------------------------------------------------------------

/// Catches a child starting on a schema a later release changed incompatibly. Version N+1
/// (N the newest this build embeds) is recorded as a contract step (what a later release
/// would write); child N (1..=N) must refuse with one line naming both versions, and child
/// N+1 (1..=N+1) must start.
///
/// Catches: `TooNew` check removed (the store opened on a schema it cannot run).
#[test]
fn a_schema_changed_incompatibly_by_a_newer_release_is_refused() {
    let s = Scratch::new();
    let config = s.config();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&config, &schema::embedded(), n).expect("creates");
    let row = format!(
        "INSERT INTO schema_version (version, name, kind) VALUES ({n1}, 'later', 'contract')"
    );
    s.sql(&row).expect("the later release's row");
    let why = refusal(&config.clone().with_supported_schema(1, n));
    assert!(why.contains(&format!("version {n1}")), "{why}");
    assert!(why.contains(&format!("1 to {n}")), "{why}");
    assert!(why.contains("matches the database"), "{why}");
    assert!(!why.contains('\n'), "one line: {why:?}");
    PostgresStore::open(&config.with_supported_schema(1, n1))
        .expect("the release that knows N+1 starts");
}

/// Catches a child starting on a schema older than it needs (an update whose migration did
/// not run): version 1 on a build that supports 2 to 2.
///
/// Catches: `found < supported.min` comparison inverted.
#[test]
fn a_schema_older_than_the_binary_needs_is_refused() {
    let s = Scratch::new();
    let config = s.config();
    schema::apply(&config, &two(), 1).expect("only version 1");
    let why = refusal(&config.clone().with_supported_schema(2, 2));
    assert!(
        why.contains("version 1") && why.contains("2 to 2") && why.contains("kbf-installer setup"),
        "{why}"
    );
    assert!(!why.contains('\n'), "one line: {why:?}");
    schema::apply(&config, &two(), 2).expect("the update migrates");
    PostgresStore::open(&config.with_supported_schema(2, 2)).expect("now it starts");
}

/// Catches a destructive step reaching the database. A set whose last version is a contract
/// step is refused whole (nothing, not even an earlier pending expand step, is run), while
/// the expand prefix of the same set applies when `upto` stops before it.
///
/// Catches: the `ContractPending` check removed (the DROP ran and the column was
/// gone).
#[test]
fn an_update_never_runs_a_contract_step_and_runs_nothing_when_one_is_pending() {
    let s = Scratch::new();
    let config = s.config();
    let (n1, n2) = (schema::latest() + 1, schema::latest() + 2);
    let contract = Migration::new(
        n2,
        "drop_expiry",
        "-- kbf-migration: contract\nALTER TABLE kv DROP COLUMN expires_us;",
    )
    .expect("a contract step lints");
    let mut set = two();
    set.push(contract);
    assert_eq!(
        schema::apply(&config, &set, n2).unwrap_err(),
        SchemaError::ContractPending { version: n2 }
    );
    assert_eq!(schema::current(&config), Err(SchemaError::NotInitialised));
    assert!(!s.has_schema(), "a refused update created the schema");
    schema::apply(&config, &set, n1).expect("the expand prefix applies");
    assert_eq!(
        schema::apply(&config, &set, n2).unwrap_err(),
        SchemaError::ContractPending { version: n2 }
    );
    s.query("SELECT expires_us FROM kv");
    assert_eq!(schema::current(&config), Ok(n1));
}

/// Catches a migration that fails half way leaving half its work: the second statement of
/// the migration after the newest embedded one fails, and neither its first statement nor a
/// version row may remain.
///
/// Catches: the BEGIN left out of `run_in_transaction` (`half` existed).
#[test]
fn a_migration_that_fails_is_rolled_back_whole() {
    let s = Scratch::new();
    let config = s.config();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    let mut set = schema::embedded();
    set.push(
        Migration::new(
            n1,
            "broken",
            "-- kbf-migration: expand\nCREATE TABLE half (a int);\nALTER TABLE nosuch ADD COLUMN b int;",
        )
        .expect("lints"),
    );
    let SchemaError::Failed { version, name, why } = schema::apply(&config, &set, n1).unwrap_err()
    else {
        panic!("a failed migration")
    };
    assert_eq!((version, name.as_str()), (n1, "broken"));
    assert!(why.contains("nosuch"), "{why}");
    assert_eq!(schema::current(&config), Ok(n));
    assert!(
        s.query("SELECT to_regclass('half')::text")[0]
            .get::<_, Option<String>>(0)
            .is_none()
    );
}

/// Catches a database ahead of the installer being "migrated" backwards or ignored: the
/// package is older than the schema it is asked to serve.
#[test]
fn an_installer_older_than_the_schema_refuses_and_a_release_it_cannot_serve_is_refused() {
    let s = Scratch::new();
    let config = s.config();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&config, &two(), n1).expect("version N+1");
    assert_eq!(
        schema::apply(&config, &schema::embedded(), n).unwrap_err(),
        SchemaError::DatabaseAhead {
            found: n1,
            newest: n
        }
    );
    assert_eq!(
        schema::apply(&config, &two(), n1 + 1).unwrap_err(),
        SchemaError::NotShipped {
            wanted: n1 + 1,
            newest: n1
        }
    );
}

/// Catches a set that skips or reorders versions, and a database whose rows skip one.
#[test]
fn numbering_gaps_are_refused_in_the_set_and_in_the_database() {
    let s = Scratch::new();
    let config = s.config();
    let n1 = schema::latest() + 1;
    assert_eq!(
        schema::apply(&config, &[second()], n1).unwrap_err(),
        SchemaError::Numbering {
            expected: 1,
            found: n1
        }
    );
    schema::apply(&config, &schema::embedded(), 1).expect("creates");
    let row = "INSERT INTO schema_version (version, name, kind) VALUES (3, 'x', 'expand')";
    s.sql(row).expect("a skipped row");
    assert_eq!(
        schema::apply(&config, &two(), 2).unwrap_err(),
        SchemaError::Gap {
            before: 1,
            after: 3
        }
    );
}

/// Catches tables from before schemas were versioned (a U1 dev database) being adopted:
/// the fresh-start ruling means migration 1 must fail, loudly, and record nothing.
///
/// Catches: migration 1 written with `CREATE TABLE IF NOT EXISTS` (it "succeeded"
/// over the old tables).
#[test]
fn tables_from_before_the_schema_was_versioned_are_refused() {
    let s = Scratch::new();
    let old = format!("CREATE SCHEMA {SCHEMA}; CREATE TABLE {SCHEMA}.kv (key text PRIMARY KEY)");
    s.sql(&old).expect("an old store");
    let SchemaError::Failed { why, .. } =
        schema::apply(&s.config(), &schema::embedded(), 1).unwrap_err()
    else {
        panic!("a failed migration")
    };
    assert!(
        why.contains("already exists") && why.contains("before the schema was versioned"),
        "{why}"
    );
    assert_eq!(
        schema::current(&s.config()),
        Err(SchemaError::NotInitialised)
    );
}

/// Catches two installers (or an installer and a second frontend host's setup) racing:
/// four threads apply to one empty schema; all succeed, version 1 is recorded once, and
/// exactly one of them ran it.
///
/// Catches: the advisory lock removed (a thread failed on a duplicate key or on
/// `CREATE TABLE` already existing).
#[test]
fn installers_that_race_apply_a_migration_once() {
    let s = Scratch::new();
    let config = s.config();
    let ran: Vec<Vec<u32>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    schema::apply(&config, &schema::embedded(), 1)
                        .expect("every racer succeeds")
                        .ran
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("racer"))
            .collect()
    });
    assert_eq!(ran.iter().map(Vec::len).sum::<usize>(), 1, "{ran:?}");
    assert_eq!(s.query("SELECT version FROM schema_version").len(), 1);
}

// ---- bounded waits ----------------------------------------------------------------------

/// A session that holds the schema lock of the database at `url`, as an installer in the
/// middle of a migration does. The lock is one per database, whatever the schema: the tests
/// below hold it on purpose, each in the database of its own, so no other test waits on it.
struct LockHolder(Client);

impl LockHolder {
    fn take(url: &str) -> LockHolder {
        let mut client = Client::connect(url, NoTls).expect("connects");
        client
            .batch_execute("BEGIN; SELECT pg_advisory_xact_lock(hashtext('kbf-state-schema'))")
            .expect("the lock is free");
        LockHolder(client)
    }

    fn release(mut self) {
        self.0.batch_execute("ROLLBACK").expect("released");
    }
}

fn apply_in_thread(
    config: PostgresConfig,
    set: Vec<Migration>,
    upto: u32,
    wait: Duration,
) -> mpsc::Receiver<Result<schema::Outcome, SchemaError>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(schema::apply_within(&config, &set, upto, wait));
    });
    rx
}

/// Catches a migration that waits for ever for an installer that holds the schema lock (a
/// swap would hang with its streams held), and one that gives up and leaves a trace. With
/// the lock held, a short wait ends in `Busy` after about the wait and creates nothing; a
/// long wait stays blocked until the holder lets go and then runs; a third call is a no-op.
///
/// Catches: the `SET LOCAL lock_timeout` left out (the short wait never ends: the
/// 5 s receive times out); `Busy` mapped to a plain database error.
#[test]
fn a_migration_waits_for_the_schema_lock_for_as_long_as_it_is_told() {
    let s = Scratch::new();

    let holder = LockHolder::take(&s.url());
    let started = Instant::now();
    let short = apply_in_thread(s.config(), two(), 2, Duration::from_millis(300));
    let gave_up = short.recv_timeout(Duration::from_secs(5));
    let waited = started.elapsed();
    holder.release();
    assert_eq!(
        gave_up.expect("it gives up").unwrap_err(),
        SchemaError::Busy
    );
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    assert!(!s.has_schema(), "a migration that waited left a schema");

    let holder = LockHolder::take(&s.url());
    let long = apply_in_thread(s.config(), two(), 2, Duration::from_secs(60));
    assert!(
        long.recv_timeout(Duration::from_millis(500)).is_err(),
        "the migration ran under a held lock"
    );
    holder.release();
    let out = long
        .recv_timeout(Duration::from_secs(30))
        .expect("it runs once the lock is free")
        .expect("it applies");
    assert_eq!((out.from, out.to, out.ran), (None, 2, vec![1, 2]));

    let again = schema::apply_within(&s.config(), &two(), 2, Duration::from_secs(30)).unwrap();
    assert!(again.ran.is_empty(), "{again:?}");
}

/// Catches the wait covering the schema lock only: the old child holds a long read on `kv`
/// (the table migration N+1 alters, N the newest this build embeds), so the `ALTER` would
/// queue for an exclusive lock and stall every query behind it. It must give up as `Busy`,
/// roll back, and leave the schema at version N with the old child's read undisturbed.
///
/// Catches: the lock timeout reset (`SET LOCAL lock_timeout = 0`) right after the
/// schema lock, so only the advisory wait was bounded: the `ALTER` waited past the 5 s.
#[test]
fn a_migration_gives_up_when_a_table_it_alters_is_in_use() {
    let s = Scratch::new();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&s.config(), &schema::embedded(), n).expect("version N");
    let mut reader = Client::connect(&s.url(), NoTls).expect("connects");
    let read = format!("BEGIN; SET LOCAL search_path TO {SCHEMA}; SELECT count(*) FROM kv");
    reader.batch_execute(&read).expect("a read in progress");
    let mut set = schema::embedded();
    set.push(
        Migration::new(
            n1,
            "half",
            "-- kbf-migration: expand\nCREATE TABLE half (a int);\nALTER TABLE kv ADD COLUMN note text;",
        )
        .expect("lints"),
    );
    let rx = apply_in_thread(s.config(), set, n1, Duration::from_millis(300));
    let got = rx.recv_timeout(Duration::from_secs(5));
    let still_reading = reader.query("SELECT count(*) FROM kv", &[]).is_ok();
    reader.batch_execute("ROLLBACK").expect("read ends");
    assert_eq!(got.expect("it gives up").unwrap_err(), SchemaError::Busy);
    assert!(still_reading);
    assert_eq!(schema::current(&s.config()), Ok(n));
    assert_eq!(
        s.query("SELECT count(*) FROM information_schema.tables WHERE table_name = 'half' AND table_schema = current_schema()")[0]
            .get::<_, i64>(0),
        0,
        "the first statement of the rolled back migration remained"
    );
}

// ---- refused connections ----------------------------------------------------------------

/// Catches the server's FATAL text, which quotes role, database and host, reaching a schema
/// error: a database that does not exist is refused with its SQLSTATE and not its name, in
/// `apply`, in `current` and in `Debug`.
///
/// Catches: `describe_refusal` returning the server's text.
#[test]
fn a_refused_connection_does_not_name_the_database() {
    let s = Scratch::new();
    let url = s.url();
    let (base, _) = url.rsplit_once('/').expect("a URL");
    let config = PostgresConfig::parse(&format!("{base}/kbf_nodb_marker")).expect("parses");
    let err = schema::apply(&config, &schema::embedded(), 1).unwrap_err();
    let SchemaError::Database(why) = &err else {
        panic!("a database error: {err:?}")
    };
    assert!(why.contains("SQLSTATE 3D000"), "{why}");
    for text in [
        err.to_string(),
        format!("{err:?}"),
        format!("{:?}", schema::current(&config)),
    ] {
        assert!(!text.contains("kbf_nodb_marker"), "{text}");
    }
}

/// Catches a server refusal in the migration's opening statements (here: the schema table
/// cannot be made in the system catalog) being reported as `Busy`, which would tell the
/// swap to retry something that never will work. It is a plain database error.
///
/// Catches: `Busy` returned for every error of the opening statements.
#[test]
fn a_refusal_of_the_opening_statements_is_not_busy() {
    let s = Scratch::new();
    let config = PostgresConfig::parse(&s.url())
        .and_then(|c| c.with_schema("pg_catalog"))
        .expect("parses");
    let err =
        schema::apply_within(&config, &schema::embedded(), 1, Duration::from_secs(5)).unwrap_err();
    assert!(matches!(err, SchemaError::Database(_)), "{err:?}");
}
