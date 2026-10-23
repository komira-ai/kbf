//! `test_database`: a database of its own for one test, with the schema `kbf-installer setup`
//! leaves. The test server is the one `KBF_TEST_POSTGRES_URL` names.
#![cfg(feature = "postgres")]

use kbf_state::postgres::schema;
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase, test_database};
use kbf_state::{Cond, Store, Txn};

/// A store opened on `database`'s schema, as a flag-opened store is.
fn store_on(database: &TestDatabase) -> PostgresStore {
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    PostgresStore::open(&config).expect("a store opens on the schema")
}

fn put(store: &PostgresStore, key: &str) {
    store
        .txn(Txn::new().put(key, "v", Cond::Any))
        .expect("reachable")
        .expect("commits");
}

/// Asserts that a test database holds the state schema at its newest version, with the
/// default schema name, and that a store opens on it, writes and reads back.
#[test]
fn a_test_database_has_the_state_schema_and_a_store_runs_on_it() {
    let database = test_database();
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    assert_eq!(schema::current(&config), Ok(schema::latest()));
    assert_eq!(config.schema(), PostgresConfig::DEFAULT_SCHEMA);
    let store = store_on(&database);
    put(&store, "k");
    assert!(store.get("k").expect("reads").is_some());
}

/// Asserts that two test databases hold stores that do not share a row: a key written through
/// the first is read back there and is absent from the second.
#[test]
fn two_test_databases_hold_separate_stores() {
    let first = test_database();
    let second = test_database();
    assert_ne!(first.name(), second.name());
    let (in_first, in_second) = (store_on(&first), store_on(&second));
    put(&in_first, "only-in-the-first");
    assert!(in_first.get("only-in-the-first").expect("reads").is_some());
    assert!(in_second.get("only-in-the-first").expect("reads").is_none());
}
