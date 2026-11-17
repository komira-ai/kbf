//! A database that applied the pack index's migration while it was part of the state store
//! (the rows the state store's runner recorded) is up to date for the runner that now takes
//! the migration from this crate: nothing is run again, nothing is refused, the rows are
//! untouched, and the index and the store open on it.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a database
//! they may create and drop in.
#![cfg(feature = "postgres")]

use kbf_pack_index::action::ActionStore;
use kbf_pack_index::postgres::PostgresPackIndex;
use kbf_pack_index::schema::{self, SQL};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex};
use kbf_state::postgres::schema as state_schema;
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase};
use postgres::{Client, NoTls};

/// The rows of `schema_version` as text, with the time each was applied.
fn recorded(client: &mut Client) -> Vec<(i32, String, String, String)> {
    let sql = "SELECT version, name, kind, applied_at::text \
               FROM kbf_state.schema_version ORDER BY version";
    let rows = client.query(sql, &[]).expect("the version table");
    rows.iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect()
}

/// A database as the state store's own runner left it when the pack index was part of it:
/// migration 1 applied, then the file of migration 2 run and its row inserted as that runner
/// did (version, name, kind), written here without the runner of today.
fn a_database_the_old_runner_migrated() -> (TestDatabase, PostgresConfig) {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    state_schema::apply(&config, &state_schema::embedded(), 1).expect("migration 1");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    client
        .batch_execute(&format!(
            "BEGIN; SET LOCAL search_path TO kbf_state; {SQL}; \
             INSERT INTO schema_version (version, name, kind) \
             VALUES (2, 'pack_index', 'expand'); COMMIT"
        ))
        .expect("migration 2 as it was applied");
    (db, config)
}

/// Asserts that the runner over the set this crate extends recognises the old runner's rows:
/// up to migration 2 it runs nothing and says the schema is at 2, the rows (their names, kinds and times) are as
/// they were, the unit's boot step leaves the database alone, and both the pack index and the
/// state store open on it. Catches: a changed number, name or file that makes the migration
/// look new (it would run again and fail on the existing tables), and an extension that
/// renumbers the base.
#[test]
fn a_database_the_old_runner_migrated_is_up_to_date() {
    let (db, config) = a_database_the_old_runner_migrated();
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let before = recorded(&mut client);
    assert_eq!(before.len(), 2);
    let out = state_schema::apply(&config, &schema::embedded(), schema::VERSION).expect("applies");
    assert_eq!((out.from, out.to), (Some(2), 2));
    assert!(out.ran.is_empty(), "{out:?}");
    assert_eq!(recorded(&mut client), before);
    let said = schema::boot_step(config.clone())
        .migrate_to(schema::VERSION)
        .expect("the unit may start");
    assert_eq!(said, "the schema is at version 2, nothing to apply");
    assert_eq!(recorded(&mut client), before);
    let index = PostgresPackIndex::open(&config).expect("the index opens");
    assert_eq!(index.pack("no-such-pack"), Ok(None));
    PostgresStore::open(&config).expect("the store opens");
}

/// Asserts that a database at migration 1 only (a release before the pack index) is brought to
/// 2 by the extended set, running exactly the pack index's migration, and that the rows are
/// then the ones the old runner would have recorded.
#[test]
fn a_database_at_migration_one_is_brought_to_two_with_the_old_rows() {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    state_schema::apply(&config, &state_schema::embedded(), 1).expect("migration 1");
    let out = state_schema::apply(&config, &schema::embedded(), schema::VERSION).expect("applies");
    assert_eq!((out.from, out.to, out.ran), (Some(1), 2, vec![2]));
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let names: Vec<(i32, String, String)> = recorded(&mut client)
        .into_iter()
        .map(|(v, n, k, _)| (v, n, k))
        .collect();
    let want = [(1, "state", "expand"), (2, "pack_index", "expand")];
    let want: Vec<(i32, String, String)> = want
        .iter()
        .map(|(v, n, k)| (*v, (*n).to_owned(), (*k).to_owned()))
        .collect();
    assert_eq!(names, want);
}

/// Asserts that a database the old runner migrated is brought to the release's latest by
/// running the action cache's migration alone: the earlier rows are untouched, and the new
/// row is the one a fresh database records.
#[test]
fn a_database_the_old_runner_migrated_takes_the_action_cache_migration_alone() {
    let (db, config) = a_database_the_old_runner_migrated();
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let before = recorded(&mut client);
    let upto = schema::ACTION_CACHE_VERSION;
    let out = state_schema::apply(&config, &schema::embedded(), upto).expect("applies");
    assert_eq!((out.from, out.to, out.ran), (Some(2), upto, vec![3]));
    let after = recorded(&mut client);
    assert_eq!(after[..2], before[..]);
    let names: Vec<(i32, &str, &str)> = after
        .iter()
        .map(|(v, n, k, _)| (*v, n.as_str(), k.as_str()))
        .collect();
    assert_eq!(names[2..], [(3, "action_cache", "expand")]);
}

/// Asserts that a database at the action cache's migration (the release before the Directory
/// rows) is brought to the release's latest by running the directory migration alone: the
/// earlier rows are untouched, the new row is `directory`, an expand step, and the pack and
/// action rows written before are still read.
#[test]
fn a_database_at_the_action_cache_takes_the_directory_migration_alone() {
    let (db, config) = a_database_the_old_runner_migrated();
    state_schema::apply(&config, &schema::embedded(), schema::ACTION_CACHE_VERSION)
        .expect("the release before the directory rows");
    let blob = BlobDigest {
        hash: [9; 32],
        size: 10,
    };
    let pack = NewPack {
        key: "cas/f/fe/1/1".to_owned(),
        writer: "f/fe".to_owned(),
        size: 26,
        blobs: vec![PackBlob {
            digest: blob,
            offset: 0,
        }],
    };
    let index = PostgresPackIndex::open(&config).expect("opens at migration 3");
    index
        .put_action(&blob, b"result", std::slice::from_ref(&pack))
        .expect("the older release writes");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let before = recorded(&mut client);
    let out = state_schema::apply(&config, &schema::embedded(), schema::latest()).expect("applies");
    assert_eq!(
        (out.from, out.to, out.ran),
        (Some(3), schema::latest(), vec![4])
    );
    let located = index.locate(&[blob]).expect("locates after the migration");
    assert_eq!(
        located[0].as_ref().map(|l| l.pack_key.as_str()),
        Some("cas/f/fe/1/1")
    );
    let action = index.get_action(&blob).expect("reads after the migration");
    assert_eq!(action.map(|a| a.result), Some(b"result".to_vec()));
    let after = recorded(&mut client);
    assert_eq!(after[..3], before[..]);
    let names: Vec<(i32, &str, &str)> = after
        .iter()
        .map(|(v, n, k, _)| (*v, n.as_str(), k.as_str()))
        .collect();
    assert_eq!(names[3..], [(4, "directory", "expand")]);
}
