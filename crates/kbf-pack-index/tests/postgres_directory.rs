//! The Directory rows on a real Postgres server: the conformance clauses D1 to D7 (the clauses
//! `tests/directory_memory.rs` runs against the in-memory index), and what is Postgres's alone:
//! a schema without the table says so until the installer has run, a record takes none of the
//! locks a state transaction holds, rows carry the database's clock when none is injected, and
//! the table is the one the rows are specified by.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a database
//! they may create and drop schemas in. Waits are on conditions; no assertion depends on
//! elapsed time.
#![cfg(feature = "postgres")]

use std::thread;

use kbf_pack_index::directory_conformance::{
    d1_a_recorded_directory_is_read_back, d2_a_record_repeats,
    d3_bytes_that_are_not_the_digest_are_refused, d4_bodies_and_batches_are_bounded,
    d5_an_empty_call_is_an_empty_answer, d6_concurrent_records_agree,
    d7_directory_rows_are_not_blob_rows, directory,
};
use kbf_pack_index::postgres::{PostgresPackBackend, PostgresPackIndex};
use kbf_pack_index::schema::{self as pack_schema, test_database};
use kbf_pack_index::{PackIndex, PackIndexError};
use kbf_state::postgres::schema;
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase};
use kbf_state::{Cond, Txn};
use postgres::error::SqlState;
use postgres::{Client, NoTls};

fn backend() -> PostgresPackBackend {
    PostgresPackBackend::new(&kbf_testkit::database::server_url())
}

// ---- the suite -------------------------------------------------------------------------

/// Asserts that recorded Directories are read back as their bytes in the order asked, and that
/// a digest never recorded, or one of the same hash and another size, has no row.
#[test]
fn d1_a_recorded_directory_is_read_back_in_order() {
    d1_a_recorded_directory_is_read_back(&backend());
}

/// Asserts that a repeated record inserts nothing and keeps the bytes, and that a digest named
/// twice in one call is one row.
#[test]
fn d2_a_repeated_record_inserts_nothing() {
    d2_a_record_repeats(&backend());
}

/// Asserts that a body whose hash or length is not its digest refuses the whole call and
/// writes none of it.
#[test]
fn d3_a_body_that_is_not_its_digest_writes_nothing() {
    d3_bytes_that_are_not_the_digest_are_refused(&backend());
}

/// Asserts the body bound and the batch bound, each at exactly the bound and one past it.
#[test]
fn d4_the_body_and_the_batch_are_bounded() {
    d4_bodies_and_batches_are_bounded(&backend());
}

/// Asserts that an empty record and an empty read answer nothing.
#[test]
fn d5_an_empty_call_answers_nothing() {
    d5_an_empty_call_is_an_empty_answer(&backend());
}

/// Asserts that two connections recording the same Directories at once in opposite orders
/// both succeed and insert each row once between them.
#[test]
fn d6_two_connections_record_the_same_directories_at_once() {
    d6_concurrent_records_agree(&backend());
}

/// Asserts that a Directory row does not make its blob locatable or present.
#[test]
fn d7_a_directory_row_is_not_a_blob_row() {
    d7_directory_rows_are_not_blob_rows(&backend());
}

// ---- Postgres alone ----------------------------------------------------------------------

/// Asserts that a schema at migration 3 (no directory table) still opens for the pack index,
/// that both Directory calls on it are `NotMigrated` with the line that says what to run, and
/// that both work on the same index once the installer's migration has run.
#[test]
fn a_schema_without_the_directory_table_is_refused_until_the_installer_has_run() {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    schema::apply(
        &config,
        &pack_schema::embedded(),
        pack_schema::ACTION_CACHE_VERSION,
    )
    .expect("the release before the directory rows");
    let index = PostgresPackIndex::open(&config).expect("opens without the directory table");
    let one = directory(b"dir-one");
    assert_eq!(
        index.directories(&[one.digest]),
        Err(PackIndexError::NotMigrated)
    );
    let refused = index
        .record_directories(std::slice::from_ref(&one))
        .expect_err("no table");
    assert_eq!(refused, PackIndexError::NotMigrated);
    assert!(refused.to_string().contains("kbf-installer setup"));
    schema::apply(&config, &pack_schema::embedded(), pack_schema::latest())
        .expect("the installer's step");
    assert_eq!(index.record_directories(std::slice::from_ref(&one)), Ok(1));
    assert_eq!(index.directories(&[one.digest]), Ok(vec![Some(one.body)]));
}

/// Asserts that a record goes through while a state transaction holds the commit head (a
/// third connection asks for the head without waiting and is refused): the Directory rows take
/// none of the locks the state store's transactions queue on. The server's lock timeout (30 s)
/// only stops a test that would otherwise hang if a record waited for the head; nothing here
/// measures time.
#[test]
fn recording_directories_goes_through_while_a_state_transaction_holds_the_head() {
    let url = {
        let base = kbf_testkit::database::server_url();
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}options=-c%20lock_timeout%3D30000")
    };
    let backend = PostgresPackBackend::new(&url);
    let config = backend.fresh_config();
    let store = PostgresStore::open(&config).expect("opens");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let held = store
        .begin(Txn::new().put("k", "v", Cond::Any))
        .expect("runs up to its commit");
    let mut raw = Client::connect(&url, NoTls).expect("connects");
    let sql = format!("SELECT rev FROM {}.head FOR UPDATE NOWAIT", config.schema());
    let refused = raw.query(&sql, &[]).expect_err("the head is held");
    assert_eq!(refused.code(), Some(&SqlState::LOCK_NOT_AVAILABLE));
    let one = directory(b"dir-one");
    let recorded = thread::scope(|scope| {
        scope
            .spawn(|| index.record_directories(std::slice::from_ref(&one)))
            .join()
            .expect("the record thread did not panic")
    });
    assert_eq!(recorded, Ok(1), "the record goes through");
    held.commit().expect("the state transaction commits");
    assert_eq!(index.directories(&[one.digest]), Ok(vec![Some(one.body)]));
}

/// Asserts that with no clock injected a row carries the database's own clock.
#[test]
fn rows_carry_the_database_clock_when_no_clock_is_injected() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let now = "SELECT floor(extract(epoch from clock_timestamp()) * 1000000)::bigint";
    let before: i64 = client.query_one(now, &[]).expect("the clock").get(0);
    index
        .record_directories(&[directory(b"dir-one")])
        .expect("record");
    let after: i64 = client.query_one(now, &[]).expect("the clock").get(0);
    let sql = "SELECT written_us FROM kbf_state.directory";
    let written: i64 = client.query_one(sql, &[]).expect("the row").get(0);
    assert!(
        before <= written && written <= after,
        "{before} {written} {after}"
    );
}

/// Asserts that the table is the one the rows are specified by: the columns and their types, a
/// primary key of hash and size, and the body stored as the bytes recorded.
#[test]
fn the_table_has_the_columns_and_key_of_the_directory_rows() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let sql = "SELECT column_name::text, data_type::text FROM information_schema.columns \
               WHERE table_schema = 'kbf_state' AND table_name = 'directory' \
               ORDER BY ordinal_position";
    let columns: Vec<(String, String)> = client
        .query(sql, &[])
        .expect("the catalog")
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let want = [
        ("hash", "bytea"),
        ("size", "bigint"),
        ("body", "bytea"),
        ("written_us", "bigint"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(c, t)| ((*c).to_owned(), (*t).to_owned()))
        .collect();
    assert_eq!(columns, want);
    let sql = "SELECT indexdef FROM pg_indexes \
               WHERE schemaname = 'kbf_state' AND tablename = 'directory'";
    let indexes: Vec<String> = client
        .query(sql, &[])
        .expect("the catalog")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(indexes.len(), 1, "{indexes:?}");
    assert!(indexes[0].contains("(hash, size)"), "{indexes:?}");
    let one = directory(b"dir-one");
    index
        .record_directories(std::slice::from_ref(&one))
        .expect("record");
    let sql = "SELECT body FROM kbf_state.directory WHERE hash = $1 AND size = $2";
    let size = i64::try_from(one.digest.size).expect("small");
    let body: Vec<u8> = client
        .query_one(sql, &[&one.digest.hash.to_vec(), &size])
        .expect("the row")
        .get(0);
    assert_eq!(body, one.body);
}
