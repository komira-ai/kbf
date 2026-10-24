//! The schema step of `kbf boot-check` against a real server: a database with no schema, one
//! behind the build, one at the build's schema, one ahead of it by an expand step, one with a
//! contract step above it, a pending migration that is a contract step, and a schema lock held
//! by another session. Each test makes a schema of its own and
//! drops it.
//!
//! The tests run in every test run (none is `#[ignore]`d), and fail, not skip, when
//! `KBF_TEST_POSTGRES_URL` names no server; it names a database they may create and drop
//! schemas in.
#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_state::StoreError;
use kbf_state::postgres::boot::{BootStep, LOCK_WAIT};
use kbf_state::postgres::schema::{self, Migration, SchemaError};
use kbf_state::postgres::{PostgresConfig, PostgresStore};
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

static NEXT: AtomicU64 = AtomicU64::new(0);

fn url() -> String {
    std::env::var(URL_VAR)
        .unwrap_or_else(|_| panic!("{URL_VAR} must name a postgres:// URL the tests may use"))
}

/// A schema name no other test uses, dropped with the guard.
struct Scratch {
    name: String,
}

impl Scratch {
    fn new() -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Scratch {
            name: format!("kbf_b{}_{n}", std::process::id()),
        }
    }

    fn config(&self) -> PostgresConfig {
        PostgresConfig::parse(&url())
            .and_then(|c| c.with_schema(&self.name))
            .expect("the test URL parses")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(&url(), NoTls) {
            let _ = client.batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.name));
        }
    }
}

/// The migrations of a build that embeds `extra` after this build's.
fn with(extra: Migration) -> Vec<Migration> {
    let mut set = schema::embedded();
    set.push(extra);
    set
}

/// The migration after the newest of this build, in the build that is one release ahead: only
/// adds a column.
fn expand() -> Migration {
    Migration::new(
        schema::latest() + 1,
        "note",
        "-- kbf-migration: expand\nALTER TABLE kv ADD COLUMN note text;",
    )
    .expect("an additive migration lints")
}

/// The migration after the newest of this build, in a build that removes what an earlier
/// release stopped using.
fn contract() -> Migration {
    Migration::new(
        schema::latest() + 1,
        "tidy",
        "-- kbf-migration: contract\nSELECT 1;",
    )
    .expect("a contract migration lints")
}

/// The schema `s` is at, as the step's own reader says.
fn at(s: &Scratch) -> Result<u32, SchemaError> {
    schema::current(&s.config())
}

/// Asserts that a database with no schema gets the schema of this build before the unit
/// starts, and that a store then opens on it.
#[test]
fn a_database_with_no_schema_gets_it_and_a_store_opens_on_it() {
    let s = Scratch::new();
    let step = BootStep::of_this_build(s.config());
    let latest = schema::latest();
    let said = step.migrate_to(latest).expect("creates");
    let want = format!("created the schema at version {latest}");
    assert_eq!(said, want);
    assert_eq!(at(&s), Ok(latest));
    PostgresStore::open(&s.config()).expect("a store opens on what the step made");
}

/// Asserts that a database one migration behind the build is migrated to the build's schema,
/// and that a store of the build opens on it: the unit starts on the schema it needs.
#[test]
fn a_schema_behind_the_build_is_migrated_before_the_unit_starts() {
    let s = Scratch::new();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&s.config(), &schema::embedded(), n).expect("the earlier release is installed");
    assert_eq!(at(&s), Ok(n));
    let step = BootStep::new(s.config(), with(expand()), LOCK_WAIT);
    let said = step.migrate_to(n1).expect("migrates");
    let want = format!("migrated the schema from {n} to {n1}");
    assert_eq!(said, want);
    assert_eq!(at(&s), Ok(n1));
    let build = s.config().with_supported_schema(1, n1);
    PostgresStore::open(&build).expect("a store of the later release opens");
}

/// Asserts that a database already at the build's schema is left alone: the step says there is
/// nothing to apply, and the schema is where it was.
#[test]
fn a_schema_at_the_build_is_left_alone() {
    let s = Scratch::new();
    let latest = schema::latest();
    schema::apply(&s.config(), &schema::embedded(), latest).expect("installed");
    let step = BootStep::of_this_build(s.config());
    let said = step.migrate_to(latest).expect("nothing to do");
    let want = format!("the schema is at version {latest}, nothing to apply");
    assert_eq!(said, want);
    assert_eq!(at(&s), Ok(latest));
}

/// Asserts that a database ahead of the build by an expand step is left as it was and is not a
/// refusal: the step says nothing was applied and names no installer package, the schema is
/// still the later one, and a store of the build opens on it. A kbf package rolled back to an
/// earlier release starts on the schema its successor left.
#[test]
fn a_schema_ahead_of_the_build_by_an_expand_step_is_left_alone() {
    let s = Scratch::new();
    let n1 = schema::latest() + 1;
    schema::apply(&s.config(), &with(expand()), n1).expect("the later release is installed");
    assert_eq!(at(&s), Ok(n1));
    let step = BootStep::of_this_build(s.config());
    let said = step.migrate_to(schema::latest()).expect("nothing to apply");
    assert!(said.contains("nothing to apply"), "{said}");
    assert!(!said.contains("installer"), "{said}");
    assert_eq!(at(&s), Ok(n1));
    PostgresStore::open(&s.config()).expect("the build serves what an expand step added");
}

/// Asserts that a database with a contract step above the build is left as it was by the step,
/// and that a store of the build refuses to open on it with the line that names the release
/// to run: the unit does not serve a schema something was removed from.
#[test]
fn a_contract_step_above_the_build_is_refused_when_the_store_opens() {
    let s = Scratch::new();
    let n1 = schema::latest() + 1;
    schema::apply(&s.config(), &schema::embedded(), schema::latest()).expect("installed");
    let mut client = Client::connect(&url(), NoTls).expect("connects");
    let row = format!(
        "INSERT INTO {}.schema_version (version, name, kind) VALUES ({n1}, 'later', 'contract')",
        s.name
    );
    client.batch_execute(&row).expect("the later release's row");
    let step = BootStep::of_this_build(s.config());
    step.migrate_to(schema::latest()).expect("nothing to apply");
    assert_eq!(at(&s), Ok(n1));
    match PostgresStore::open(&s.config()) {
        Err(StoreError::Failed(why)) => assert!(why.contains("matches the database"), "{why}"),
        Err(other) => panic!("not a failure: {other:?}"),
        Ok(_) => panic!("the store opened"),
    }
}

/// Asserts that a pending migration that is a contract step is refused and not run: an update
/// never removes anything, so the schema stays where it was.
#[test]
fn a_pending_contract_step_is_refused_and_not_run() {
    let s = Scratch::new();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&s.config(), &schema::embedded(), n).expect("installed");
    let step = BootStep::new(s.config(), with(contract()), LOCK_WAIT);
    let why = step.migrate_to(n1).expect_err("a contract step");
    assert!(why.contains("contract step"), "{why}");
    assert_eq!(at(&s), Ok(n));
}

/// Asserts that a schema lock held by another session ends the step with the busy refusal after
/// its wait (200 ms here) and applies nothing, and that once the lock is let go the same step
/// migrates: the unit is not held for ever behind another migration.
#[test]
fn a_held_schema_lock_is_a_refusal_after_the_wait() {
    let s = Scratch::new();
    let (n, n1) = (schema::latest(), schema::latest() + 1);
    schema::apply(&s.config(), &schema::embedded(), n).expect("installed");
    let mut other = Client::connect(&url(), NoTls).expect("connects");
    let mut held = other.transaction().expect("begins");
    let lock = "SELECT pg_advisory_xact_lock(hashtext('kbf-state-schema'))";
    held.batch_execute(lock).expect("holds the lock");
    let step = BootStep::new(s.config(), with(expand()), Duration::from_millis(200));
    let why = step.migrate_to(n1).expect_err("busy");
    assert_eq!(why, SchemaError::Busy.to_string());
    assert_eq!(at(&s), Ok(n));
    held.commit().expect("lock let go");
    // The lock is one key on the whole server, and the other tests take it while they migrate
    // their own schemas in parallel: once this test lets go, the step is given time to wait
    // them out.
    let step = BootStep::new(s.config(), with(expand()), LOCK_WAIT);
    let said = step.migrate_to(n1).expect("migrates");
    let want = format!("migrated the schema from {n} to {n1}");
    assert_eq!(said, want);
}
