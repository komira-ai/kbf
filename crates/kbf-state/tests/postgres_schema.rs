//! The schema the installer owns, against a real server: creating it, migrating it under
//! a serving store, and the refusals. Each test makes a schema of its own and drops it.
//!
//! The tests that need a server are `#[ignore]`d; the `state-postgres` job runs them with
//! `--include-ignored`. `KBF_TEST_POSTGRES_URL` names a database they may create and drop
//! schemas in.
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use kbf_state::postgres::schema::{self, Migration, SchemaError};
use kbf_state::postgres::{PostgresConfig, PostgresStore};
use kbf_state::{Cond, Store, StoreError, Txn};
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

static NEXT: AtomicU64 = AtomicU64::new(0);

fn url() -> String {
    std::env::var(URL_VAR)
        .unwrap_or_else(|_| panic!("{URL_VAR} must name a postgres:// URL the tests may use"))
}

fn sql(statement: &str) -> Result<(), postgres::Error> {
    Client::connect(&url(), NoTls)?.batch_execute(statement)
}

/// A schema name no other test uses, dropped with the guard.
struct Scratch {
    name: String,
}

impl Scratch {
    fn new() -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Scratch {
            name: format!("kbf_s{}_{n}", std::process::id()),
        }
    }

    fn config(&self) -> PostgresConfig {
        PostgresConfig::parse(&url())
            .and_then(|c| c.with_schema(&self.name))
            .expect("the test URL parses")
    }

    fn query(&self, statement: &str) -> Vec<postgres::Row> {
        let mut client = Client::connect(&url(), NoTls).expect("connects");
        client
            .batch_execute(&format!("SET search_path TO {}", self.name))
            .ok();
        client.query(statement, &[]).expect("the query runs")
    }

    fn has_schema(&self) -> bool {
        let mut client = Client::connect(&url(), NoTls).expect("connects");
        !client
            .query(
                "SELECT 1 FROM pg_namespace WHERE nspname = $1",
                &[&self.name],
            )
            .expect("catalog")
            .is_empty()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = sql(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.name));
    }
}

/// Release N+1's migration: only adds a column and a table.
fn second() -> Migration {
    Migration::new(
        2,
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
/// Mutants seen red: the schema_version INSERT left out (`current` says not initialised);
/// a table of migration 1 misnamed (the store's first statement fails).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_fresh_database_gets_the_schema_and_a_store_runs_on_it() {
    let s = Scratch::new();
    let config = s.config();
    let out = schema::apply(&config, &schema::embedded(), schema::latest()).expect("creates");
    assert_eq!((out.from, out.to, out.ran.clone()), (None, 1, vec![1]));
    assert_eq!(out.to_string(), "created the schema at version 1");
    assert_eq!(schema::current(&config), Ok(1));
    let rows = s.query("SELECT version, name, kind FROM schema_version");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(2), "expand");
    for table in ["kv", "feed", "counters", "head"] {
        s.query(&format!("SELECT count(*) FROM {table}"));
    }
    let store = PostgresStore::open(&config).expect("a store opens on the new schema");
    put(&store, "k").expect("writes");
    assert!(store.get("k").expect("reads").is_some());
    let again = schema::apply(&config, &schema::embedded(), schema::latest()).expect("again");
    assert_eq!((again.from, again.to, again.ran), (Some(1), 1, vec![]));
}

/// Catches kbf creating tables itself: a store opened on a database with no schema must
/// refuse with the one line that says what to run, and leave the database as it found it.
///
/// Mutant seen red: `connect` made to run the old `CREATE SCHEMA IF NOT EXISTS` statements
/// (the open succeeded and the schema existed afterwards).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
/// (supported 1..=1) has open connections and a writer thread running while the installer
/// applies migration 2 (an `ALTER TABLE ... ADD COLUMN` and a new table); no write may
/// fail, new connections the pool opens afterwards must work, and a restart of child N on
/// the migrated schema must start (the revert case). Child N+1 (1..=2) starts too.
///
/// Mutant seen red: the expand rule removed from `check_conn` (the restart refuses). A
/// check on every new connection would not be caught here: with the expand rule it passes
/// too, so only the first connection is judged, to save a query per connect.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn migrating_to_n_plus_1_leaves_the_serving_n_child_working() {
    let s = Scratch::new();
    let installer = s.config();
    schema::apply(&installer, &schema::embedded(), 1).expect("N is installed");
    let child_n = PostgresStore::open(&installer.clone().with_supported_schema(1, 1))
        .expect("child N starts on schema 1");
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
        let out = schema::apply(&installer, &two(), 2).expect("N+1 migrates");
        assert_eq!((out.from, out.to, out.ran), (Some(1), 2, vec![2]));
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
    assert_eq!(schema::current(&installer), Ok(2));
    assert!(child_n.get("before").expect("reads").is_some());

    let restarted = child_n
        .reopen()
        .expect("child N restarts on schema 2: only expand rows are above it");
    put(&restarted, "restarted").expect("writes");
    PostgresStore::open(&installer.with_supported_schema(1, 2)).expect("child N+1 starts");
}

// ---- refusals -------------------------------------------------------------------------

/// Catches a child starting on a schema a later release changed incompatibly. Version 2 is
/// recorded as a contract step (what a later release would write); child N (1..=1) must
/// refuse with one line naming both versions, and child N+1 (1..=2) must start.
///
/// Mutant seen red: `TooNew` check removed (the store opened on a schema it cannot run).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_schema_changed_incompatibly_by_a_newer_release_is_refused() {
    let s = Scratch::new();
    let config = s.config();
    schema::apply(&config, &schema::embedded(), 1).expect("creates");
    sql(&format!(
        "INSERT INTO {}.schema_version (version, name, kind) VALUES (2, 'later', 'contract')",
        s.name
    ))
    .expect("the later release's row");
    let why = refusal(&config.clone().with_supported_schema(1, 1));
    assert!(
        why.contains("version 2") && why.contains("1 to 1") && why.contains("matches the database"),
        "{why}"
    );
    assert!(!why.contains('\n'), "one line: {why:?}");
    PostgresStore::open(&config.with_supported_schema(1, 2))
        .expect("the release that knows 2 starts");
}

/// Catches a child starting on a schema older than it needs (an update whose migration did
/// not run): version 1 on a build that supports 2 to 2.
///
/// Mutant seen red: `found < supported.min` comparison inverted.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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

/// Catches a destructive step reaching the database. A set whose version 2 is a contract
/// step is refused whole (nothing, not even an earlier pending expand step, is run), while
/// the expand prefix of the same set applies when `upto` stops before it.
///
/// Mutant seen red: the `ContractPending` check removed (the DROP ran and the column was
/// gone).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn an_update_never_runs_a_contract_step_and_runs_nothing_when_one_is_pending() {
    let s = Scratch::new();
    let config = s.config();
    let contract = Migration::new(
        3,
        "drop_expiry",
        "-- kbf-migration: contract\nALTER TABLE kv DROP COLUMN expires_us;",
    )
    .expect("a contract step lints");
    let mut set = two();
    set.push(contract);
    assert_eq!(
        schema::apply(&config, &set, 3).unwrap_err(),
        SchemaError::ContractPending { version: 3 }
    );
    assert_eq!(schema::current(&config), Err(SchemaError::NotInitialised));
    assert!(!s.has_schema(), "a refused update created the schema");
    schema::apply(&config, &set, 2).expect("the expand prefix applies");
    assert_eq!(
        schema::apply(&config, &set, 3).unwrap_err(),
        SchemaError::ContractPending { version: 3 }
    );
    s.query("SELECT expires_us FROM kv");
    assert_eq!(schema::current(&config), Ok(2));
}

/// Catches a migration that fails half way leaving half its work: the second statement of
/// migration 2 fails, and neither its first statement nor a version row may remain.
///
/// Mutant seen red: the BEGIN left out of `run_in_transaction` (`half` existed).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_migration_that_fails_is_rolled_back_whole() {
    let s = Scratch::new();
    let config = s.config();
    let mut set = schema::embedded();
    set.push(
        Migration::new(
            2,
            "broken",
            "-- kbf-migration: expand\nCREATE TABLE half (a int);\nALTER TABLE nosuch ADD COLUMN b int;",
        )
        .expect("lints"),
    );
    let SchemaError::Database(why) = schema::apply(&config, &set, 2).unwrap_err() else {
        panic!("a database error")
    };
    assert!(
        why.contains("migration 2 (broken) failed") && why.contains("nosuch"),
        "{why}"
    );
    assert_eq!(schema::current(&config), Ok(1));
    assert!(
        s.query("SELECT to_regclass('half')::text")[0]
            .get::<_, Option<String>>(0)
            .is_none()
    );
}

/// Catches a database ahead of the installer being "migrated" backwards or ignored: the
/// package is older than the schema it is asked to serve.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn an_installer_older_than_the_schema_refuses_and_a_release_it_cannot_serve_is_refused() {
    let s = Scratch::new();
    let config = s.config();
    schema::apply(&config, &two(), 2).expect("version 2");
    assert_eq!(
        schema::apply(&config, &schema::embedded(), 1).unwrap_err(),
        SchemaError::DatabaseAhead {
            found: 2,
            newest: 1
        }
    );
    assert_eq!(
        schema::apply(&config, &two(), 3).unwrap_err(),
        SchemaError::NotShipped {
            wanted: 3,
            newest: 2
        }
    );
}

/// Catches a set that skips or reorders versions, and a database whose rows skip one.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn numbering_gaps_are_refused_in_the_set_and_in_the_database() {
    let s = Scratch::new();
    let config = s.config();
    assert_eq!(
        schema::apply(&config, &[second()], 2).unwrap_err(),
        SchemaError::Numbering {
            expected: 1,
            found: 2
        }
    );
    schema::apply(&config, &schema::embedded(), 1).expect("creates");
    sql(&format!(
        "INSERT INTO {}.schema_version (version, name, kind) VALUES (3, 'x', 'expand')",
        s.name
    ))
    .expect("a skipped row");
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
/// Mutant seen red: migration 1 written with `CREATE TABLE IF NOT EXISTS` (it "succeeded"
/// over the old tables).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn tables_from_before_the_schema_was_versioned_are_refused() {
    let s = Scratch::new();
    sql(&format!(
        "CREATE SCHEMA {n}; CREATE TABLE {n}.kv (key text PRIMARY KEY)",
        n = s.name
    ))
    .expect("an old store");
    let SchemaError::Database(why) =
        schema::apply(&s.config(), &schema::embedded(), 1).unwrap_err()
    else {
        panic!("a database error")
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
/// Mutant seen red: the advisory lock removed (a thread failed on a duplicate key or on
/// `CREATE TABLE` already existing).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
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
