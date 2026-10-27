//! The pack index on a real Postgres server: the conformance clauses P1 to P10 (the clauses
//! `tests/pack_index_memory.rs` runs against the in-memory index), and what is Postgres's
//! alone: recording takes none of the locks a state transaction holds, a schema without the
//! pack tables is refused, rows are stamped with the database's clock when none is injected,
//! and the migration that makes the tables only adds.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a database
//! they may create and drop schemas in. Waits are on conditions; no assertion depends on
//! elapsed time.
#![cfg(feature = "postgres")]

use std::thread;
use std::time::Duration;

use kbf_pack_index::conformance::{
    p1_record_and_locate, p2_record_is_idempotent, p3_conflicting_record_is_refused,
    p4_first_location_is_kept, p5_batches_of_a_thousand, p6_touch_changes_only_the_touch_time,
    p7_dead_packs_and_unreferenced_keys, p8_concurrent_records_all_succeed,
    p9_a_pack_that_cannot_exist_is_refused, p10_concurrent_records_of_one_pack_agree,
};
use kbf_pack_index::postgres::{PostgresPackBackend, PostgresPackIndex};
use kbf_pack_index::schema::{self as pack_schema, test_database};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex, PackIndexError};
use kbf_state::postgres::schema::{self, Kind};
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase};
use kbf_state::{Cond, Store, Txn};
use postgres::error::SqlState;
use postgres::{Client, NoTls};

/// The migration that makes the pack tables.
const PACK_MIGRATION: u32 = pack_schema::VERSION;

fn url() -> String {
    kbf_testkit::database::server_url()
}

/// The test URL with a server setting for every connection it opens.
fn url_setting(name: &str, value: &str) -> String {
    let base = url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-c%20{name}%3D{value}")
}

fn backend() -> PostgresPackBackend {
    PostgresPackBackend::new(&url())
}

fn put(store: &PostgresStore, key: &str) {
    store
        .txn(Txn::new().put(key, "v", Cond::Any))
        .expect("reachable")
        .expect("commits");
}

fn digest(n: u8) -> BlobDigest {
    BlobDigest {
        hash: [n; 32],
        size: 10,
    }
}

/// A pack of the one blob numbered `n`.
fn one_blob(key: &str, n: u8) -> NewPack {
    let blob = PackBlob {
        digest: digest(n),
        offset: 0,
    };
    NewPack {
        key: key.to_owned(),
        writer: "n1".to_owned(),
        size: 26,
        blobs: vec![blob],
    }
}

/// The database's own clock in microseconds.
fn database_now(client: &mut Client) -> i64 {
    let sql = "SELECT floor(extract(epoch from clock_timestamp()) * 1000000)::bigint";
    client.query_one(sql, &[]).expect("the clock").get(0)
}

/// One text out of the catalog.
fn text(client: &mut Client, sql: &str) -> String {
    client.query_one(sql, &[]).expect("catalog").get(0)
}

/// Fails unless `text` has `needle` in it.
#[track_caller]
fn includes(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle} is not in {text}");
}

// ---- the suite -------------------------------------------------------------------------

/// Asserts that a recorded pack is found at the offsets its footer lists, with the clock's
/// reading as its creation time.
#[test]
fn p1_a_recorded_pack_is_found() {
    p1_record_and_locate(&backend());
}

/// Asserts that recording the same pack again changes nothing.
#[test]
fn p2_recording_a_pack_again_changes_nothing() {
    p2_record_is_idempotent(&backend());
}

/// Asserts that a second record of a key with other contents is refused and changes nothing.
#[test]
fn p3_a_record_with_other_contents_is_refused() {
    p3_conflicting_record_is_refused(&backend());
}

/// Asserts that a blob already indexed in another pack keeps its first location.
#[test]
fn p4_the_first_location_of_a_blob_is_kept() {
    p4_first_location_is_kept(&backend());
}

/// Asserts that batches of a thousand digests are answered in order and a longer one is
/// refused.
#[test]
fn p5_batches_of_a_thousand_are_answered_in_order() {
    p5_batches_of_a_thousand(&backend());
}

/// Asserts that a touch sets the touch time of the blobs that have a row and nothing else.
#[test]
fn p6_a_touch_changes_only_the_touch_time() {
    p6_touch_changes_only_the_touch_time(&backend());
}

/// Asserts that the garbage-collection queries return exactly the packs that are dead after
/// the grace and the keys no pack is recorded under.
#[test]
fn p7_the_garbage_collection_queries_are_exact() {
    p7_dead_packs_and_unreferenced_keys(&backend());
}

/// Asserts that two connections recording packs that name the same blobs both succeed and
/// leave one row per blob.
#[test]
fn p8_two_connections_record_the_same_blobs_at_once() {
    p8_concurrent_records_all_succeed(&backend());
}

/// Asserts that a pack that cannot exist is refused and leaves nothing behind.
#[test]
fn p9_an_impossible_pack_is_refused() {
    p9_a_pack_that_cannot_exist_is_refused(&backend());
}

/// Asserts that two connections recording the same pack at once both succeed, exactly one of
/// them records it, and the index holds one pack with one row per blob.
#[test]
fn p10_two_connections_record_the_same_pack_at_once() {
    p10_concurrent_records_of_one_pack_agree(&backend());
}

// ---- Postgres's own parts ----------------------------------------------------------------

/// Asserts that blob rows take no part in the commit head. While a state transaction holds
/// the head (a third connection asks for it without waiting and is refused), two connections
/// record different packs and both succeed, and the blobs are found; the state transaction
/// then commits. The server's lock timeout (30 s) only stops a test that would otherwise hang
/// if a record waited for the head; nothing here measures time.
#[test]
fn recording_packs_goes_through_while_a_state_transaction_holds_the_head() {
    let backend = PostgresPackBackend::new(&url_setting("lock_timeout", "30000"));
    let config = backend.fresh_config();
    let store = PostgresStore::open(&config).expect("opens");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let other = index.reconnect().expect("opens again");
    let held = store
        .begin(Txn::new().put("k", "v", Cond::Any))
        .expect("runs up to its commit");
    let mut raw = Client::connect(&url(), NoTls).expect("connects");
    let schema = config.schema();
    let sql = format!("SELECT rev FROM {schema}.head FOR UPDATE NOWAIT");
    let refused = raw.query(&sql, &[]).expect_err("the head is held");
    assert_eq!(refused.code(), Some(&SqlState::LOCK_NOT_AVAILABLE));
    let (first, second) = thread::scope(|scope| {
        let a = scope.spawn(|| index.record_pack(&one_blob("cas/d/n1/1/1", 1)));
        let b = scope.spawn(|| other.record_pack(&one_blob("cas/d/n2/1/1", 2)));
        (
            a.join().expect("the first record did not panic"),
            b.join().expect("the second record did not panic"),
        )
    });
    let first = first.expect("the first record goes through");
    let second = second.expect("the second record goes through");
    assert_eq!((first.indexed, second.indexed), (1, 1));
    held.commit().expect("the state transaction commits");
    let asked = [digest(1), digest(2)];
    let found = index.locate(&asked).expect("located");
    assert!(found.iter().all(Option::is_some));
}

/// Asserts that a commit the server refuses (a deferred constraint fails at COMMIT) is an
/// error and not a recorded pack, that the refused pack leaves no row behind, and that the
/// index records the same pack once the constraint is gone.
#[test]
fn a_commit_the_server_refuses_is_an_error_and_leaves_nothing_behind() {
    let backend = PostgresPackBackend::new(&url());
    let config = backend.fresh_config();
    let _store = PostgresStore::open(&config).expect("opens");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let schema = config.schema();
    let mut admin = Client::connect(&url(), NoTls).expect("connects");
    admin
        .batch_execute(&format!(
            "CREATE FUNCTION {schema}.refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'commit refused by the test'; END $$; \
             CREATE CONSTRAINT TRIGGER refuse AFTER INSERT ON {schema}.pack \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.refuse()"
        ))
        .expect("a trigger that fails at commit");
    let pack = one_blob("cas/d/n1/1/1", 1);
    let refused = index.record_pack(&pack);
    assert!(
        matches!(&refused, Err(PackIndexError::Store(why)) if why.to_string().contains("commit refused by the test")),
        "{refused:?}"
    );
    admin
        .batch_execute(&format!("DROP TRIGGER refuse ON {schema}.pack"))
        .expect("removes the trigger");
    assert_eq!(
        index.pack(&pack.key),
        Ok(None),
        "the refused pack is not there"
    );
    let recorded = index
        .record_pack(&pack)
        .expect("records once the trigger is gone");
    assert!(recorded.created);
}

/// Asserts that a state schema without the pack tables is refused with the line that says what
/// to run, that the refusal creates nothing, and that the index opens once the installer's
/// migration has run.
#[test]
fn a_schema_without_the_pack_tables_is_refused_until_the_installer_has_run() {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    schema::apply(&config, &pack_schema::embedded(), 1).expect("the first migration only");
    let opened = PostgresPackIndex::open(&config);
    let refused = opened.err().expect("no tables");
    assert_eq!(refused, PackIndexError::NotMigrated);
    assert!(refused.to_string().contains("kbf-installer setup"));
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let sql = "SELECT to_regclass('kbf_state.pack')::text";
    let found: Option<String> = client.query_one(sql, &[]).expect("catalog").get(0);
    assert_eq!(found, None);
    schema::apply(&config, &pack_schema::embedded(), pack_schema::latest())
        .expect("the installer's step");
    assert!(PostgresPackIndex::open(&config).is_ok());
}

/// Asserts that with no clock injected the rows carry the database's own clock: a pack's
/// creation time and a touched blob's time lie between two readings of that clock, and a pack
/// with no blob that was recorded just now is inside a one-hour grace.
#[test]
fn rows_carry_the_database_clock_when_no_clock_is_injected() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let before = database_now(&mut client);
    let key = "cas/d/n1/1/1";
    index.record_pack(&one_blob(key, 1)).expect("recorded");
    let mut empty = one_blob("cas/d/n1/1/2", 2);
    empty.blobs.clear();
    index.record_pack(&empty).expect("recorded");
    let read = index.pack(key).expect("read");
    let created = read.expect("a pack").created_us;
    assert_eq!(index.touch(&[digest(1)]), Ok(1));
    let after = database_now(&mut client);
    let located = index.locate(&[digest(1)]).expect("located");
    let touched = located[0].as_ref().map_or(0, |l| l.last_touch_us);
    assert!(before <= created);
    assert!(created <= touched);
    assert!(touched <= after);
    let hour = Duration::from_secs(3600);
    assert_eq!(index.dead_packs(hour, 10), Ok(Vec::new()));
}

/// The `(column, type)` of `table`'s columns in the state schema, in order.
fn columns(client: &mut Client, table: &str) -> Vec<(String, String)> {
    let sql = "SELECT column_name::text, data_type::text FROM information_schema.columns \
               WHERE table_schema = 'kbf_state' AND table_name = $1 ORDER BY ordinal_position";
    let rows = client.query(sql, &[&table]).expect("the catalog");
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|p| (p.0.to_owned(), p.1.to_owned()))
        .collect()
}

/// The first release's tables.
const FIRST: [&str; 5] = ["kv", "feed", "counters", "head", "schema_version"];

fn first_columns(client: &mut Client) -> Vec<Vec<(String, String)>> {
    FIRST.iter().map(|t| columns(client, t)).collect()
}

/// Asserts that the migration is an expand step that only adds. Child 1, which knows the first
/// schema only, keeps writing and reading through it, on the connections it has and on new
/// ones, and restarts on the migrated schema; the columns of the first release's tables are as
/// they were; and the new tables have no foreign key, so none of their rows refers to the
/// key/value tables, the feed or the head.
#[test]
fn the_pack_migration_only_adds_and_the_serving_child_keeps_working() {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    schema::apply(&config, &pack_schema::embedded(), 1).expect("the first release is installed");
    let first = config.clone().with_supported_schema(1, 1);
    let child = PostgresStore::open(&first).expect("child 1 starts");
    put(&child, "before");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let old = first_columns(&mut client);
    let out = schema::apply(&config, &pack_schema::embedded(), PACK_MIGRATION).expect("the step");
    assert_eq!((out.from, out.to), (Some(1), PACK_MIGRATION));
    assert_eq!(out.ran, vec![PACK_MIGRATION]);
    let migration = pack_schema::embedded().remove(1);
    assert_eq!(migration.version(), PACK_MIGRATION);
    assert_eq!(migration.kind(), Kind::Expand);
    thread::scope(|scope| {
        for t in 0..4 {
            let child = &child;
            scope.spawn(move || put(child, &format!("after{t}")));
        }
    });
    assert_eq!(first_columns(&mut client), old);
    let restarted = child.reopen().expect("child 1 restarts");
    put(&restarted, "restarted");
    for key in ["before", "after0", "after3", "restarted"] {
        let read = restarted.get(key).expect("reads");
        assert!(read.is_some(), "{key}");
    }
    let sql = "SELECT count(*) FROM pg_constraint WHERE contype = 'f' AND conrelid IN \
               ('kbf_state.pack'::regclass, 'kbf_state.blob'::regclass)";
    let foreign: i64 = client.query_one(sql, &[]).expect("catalog").get(0);
    assert_eq!(foreign, 0);
}

/// Asserts that the tables are the ones the index is specified by: the columns and their types,
/// a unique pack key, a primary key of hash and size on the blob rows, an index of the blobs of
/// a pack, and the fillfactor that leaves room for heap-only touches.
#[test]
fn the_tables_have_the_columns_keys_and_fillfactor_of_the_index() {
    let db = test_database();
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let pack = [
        ("pack_id", "bigint"),
        ("key", "text"),
        ("writer", "text"),
        ("size", "bigint"),
        ("created_us", "bigint"),
        ("blob_count", "integer"),
        ("live_blobs", "integer"),
    ];
    assert_eq!(columns(&mut client, "pack"), owned(&pack));
    let blob = [
        ("hash", "bytea"),
        ("size", "bigint"),
        ("pack_id", "bigint"),
        ("off", "bigint"),
        ("last_touch_us", "bigint"),
    ];
    assert_eq!(columns(&mut client, "blob"), owned(&blob));
    let sql = "SELECT string_agg(indexdef, ' ') FROM pg_indexes \
               WHERE schemaname = 'kbf_state' AND tablename = 'pack'";
    let on_pack = text(&mut client, sql);
    includes(&on_pack, "UNIQUE INDEX pack_key_key");
    includes(&on_pack, "(key)");
    let sql = "SELECT string_agg(indexdef, ' ') FROM pg_indexes \
               WHERE schemaname = 'kbf_state' AND tablename = 'blob'";
    let on_blob = text(&mut client, sql);
    includes(&on_blob, "UNIQUE INDEX blob_pkey");
    includes(&on_blob, "(hash, size)");
    includes(&on_blob, "INDEX blob_pack");
    let sql = "SELECT array_to_string(reloptions, ',') FROM pg_class \
               WHERE oid = 'kbf_state.blob'::regclass";
    let options = text(&mut client, sql);
    assert_eq!(options, "fillfactor=70");
}
