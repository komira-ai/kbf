//! The action cache on a real Postgres server: the conformance clauses A1 to A9 (the clauses
//! `tests/action_memory.rs` runs against the in-memory index), and what is Postgres's alone: a
//! put takes none of the locks a state transaction holds and is one transaction with its
//! packs even when the commit is refused, a schema without the action table says so and
//! records nothing, rows carry the database's clock when none is injected, and the table is
//! the one the cache is specified by.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a database
//! they may create and drop schemas in. Waits are on conditions; no assertion depends on
//! elapsed time.
#![cfg(feature = "postgres")]

use std::thread;

use kbf_pack_index::action::{ActionEntry, ActionStore};
use kbf_pack_index::action_conformance::{
    a1_a_result_is_found_by_its_action, a2_a_put_replaces, a3_an_empty_result_is_a_result,
    a4_a_long_result_is_refused, a5_a_put_records_its_packs, a6_a_refused_pack_writes_nothing,
    a7_a_put_repeats, a8_concurrent_puts_agree, a9_a_second_handle_sees_the_row,
};
use kbf_pack_index::postgres::{PostgresPackBackend, PostgresPackIndex};
use kbf_pack_index::schema::{self as pack_schema, test_database};
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex, PackIndexError};
use kbf_state::postgres::schema;
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase};
use kbf_state::{Cond, Txn};
use postgres::error::SqlState;
use postgres::{Client, NoTls};

fn url() -> String {
    kbf_testkit::database::server_url()
}

fn backend() -> PostgresPackBackend {
    PostgresPackBackend::new(&url())
}

fn digest(n: u8) -> BlobDigest {
    BlobDigest {
        hash: [n; 32],
        size: 10,
    }
}

/// A pack of the one blob numbered `n`.
fn one_blob(key: &str, n: u8) -> NewPack {
    NewPack {
        key: key.to_owned(),
        writer: "n1".to_owned(),
        size: 26,
        blobs: vec![PackBlob {
            digest: digest(n),
            offset: 0,
        }],
    }
}

fn action(n: u8) -> BlobDigest {
    BlobDigest {
        hash: [n; 32],
        size: 77,
    }
}

/// The database's own clock in microseconds.
fn database_now(client: &mut Client) -> i64 {
    let sql = "SELECT floor(extract(epoch from clock_timestamp()) * 1000000)::bigint";
    client.query_one(sql, &[]).expect("the clock").get(0)
}

// ---- the suite -------------------------------------------------------------------------

/// Asserts that a put result is read back as put with the time of the put, and that other
/// actions, including one of the same hash and another size, have no row.
#[test]
fn a1_a_put_result_is_found_by_its_action() {
    a1_a_result_is_found_by_its_action(&backend());
}

/// Asserts that a later put replaces the row, says so and moves the time.
#[test]
fn a2_a_later_put_replaces_the_row() {
    a2_a_put_replaces(&backend());
}

/// Asserts that a result of no bytes is stored and is not read as no row.
#[test]
fn a3_an_empty_result_is_stored() {
    a3_an_empty_result_is_a_result(&backend());
}

/// Asserts that the longest result is held, a longer one is refused and a refused put records
/// no pack.
#[test]
fn a4_a_result_past_the_limit_is_refused() {
    a4_a_long_result_is_refused(&backend());
}

/// Asserts that a put records its packs, answers for them in the order given and indexes the
/// first location of a blob the packs share.
#[test]
fn a5_a_put_records_the_packs_of_its_outputs() {
    a5_a_put_records_its_packs(&backend());
}

/// Asserts that a conflicting or impossible pack leaves no row, no other pack and no change to
/// the row the action had.
#[test]
fn a6_a_refused_pack_leaves_nothing_behind() {
    a6_a_refused_pack_writes_nothing(&backend());
}

/// Asserts that a put repeated with the same packs succeeds and says each pack was recorded
/// before.
#[test]
fn a7_a_put_repeated_with_the_same_packs_succeeds() {
    a7_a_put_repeats(&backend());
}

/// Asserts that two connections putting results for the same actions and packs at once all
/// succeed, record the shared pack once and leave one of the two results per action.
#[test]
fn a8_two_connections_put_the_same_actions_at_once() {
    a8_concurrent_puts_agree(&backend());
}

/// Asserts that a row put through one connection is read through another.
#[test]
fn a9_a_row_is_committed_and_read_through_another_connection() {
    a9_a_second_handle_sees_the_row(&backend());
}

// ---- Postgres's own parts ----------------------------------------------------------------

/// Asserts that an action put takes no part in the commit head. While a state transaction
/// holds the head (a third connection asks for it without waiting and is refused), two
/// connections put different actions with different packs and both succeed, and the rows and
/// blobs are found; the state transaction then commits. The server's lock timeout (30 s) only
/// stops a test that would otherwise hang if a put waited for the head; nothing here measures
/// time.
#[test]
fn putting_an_action_goes_through_while_a_state_transaction_holds_the_head() {
    let url = {
        let base = url();
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}options=-c%20lock_timeout%3D30000")
    };
    let backend = PostgresPackBackend::new(&url);
    let config = backend.fresh_config();
    let store = PostgresStore::open(&config).expect("opens");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let other = index.reconnect().expect("opens again");
    let held = store
        .begin(Txn::new().put("k", "v", Cond::Any))
        .expect("runs up to its commit");
    let mut raw = Client::connect(&url, NoTls).expect("connects");
    let sql = format!("SELECT rev FROM {}.head FOR UPDATE NOWAIT", config.schema());
    let refused = raw.query(&sql, &[]).expect_err("the head is held");
    assert_eq!(refused.code(), Some(&SqlState::LOCK_NOT_AVAILABLE));
    let (first, second) = thread::scope(|scope| {
        let a =
            scope.spawn(|| index.put_action(&action(1), b"one", &[one_blob("cas/d/n1/1/1", 1)]));
        let b =
            scope.spawn(|| other.put_action(&action(2), b"two", &[one_blob("cas/d/n2/1/1", 2)]));
        (
            a.join().expect("the first put did not panic"),
            b.join().expect("the second put did not panic"),
        )
    });
    first.expect("the first put goes through");
    second.expect("the second put goes through");
    held.commit().expect("the state transaction commits");
    let one = index.get_action(&action(1)).expect("read");
    assert_eq!(one.map(|e| e.result), Some(b"one".to_vec()));
    let two = index.get_action(&action(2)).expect("read");
    assert_eq!(two.map(|e| e.result), Some(b"two".to_vec()));
    let found = index.locate(&[digest(1), digest(2)]).expect("located");
    assert!(found.iter().all(Option::is_some));
}

/// Asserts that a put is one transaction with its packs when the commit itself is refused (a
/// deferred constraint on the action table fails at COMMIT): the put is an error, the row and
/// the pack are not there, and the same put goes through once the constraint is gone.
#[test]
fn a_commit_the_server_refuses_leaves_neither_the_row_nor_the_packs() {
    let backend = backend();
    let config = backend.fresh_config();
    let index = PostgresPackIndex::open(&config).expect("opens");
    let schema = config.schema();
    let mut admin = Client::connect(&url(), NoTls).expect("connects");
    admin
        .batch_execute(&format!(
            "CREATE FUNCTION {schema}.refuse() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'commit refused by the test'; END $$; \
             CREATE CONSTRAINT TRIGGER refuse AFTER INSERT ON {schema}.action_result \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {schema}.refuse()"
        ))
        .expect("a trigger that fails at commit");
    let pack = one_blob("cas/d/n1/1/1", 1);
    let refused = index.put_action(&action(1), b"r", std::slice::from_ref(&pack));
    assert!(
        matches!(&refused, Err(PackIndexError::Store(why)) if why.to_string().contains("commit refused by the test")),
        "{refused:?}"
    );
    assert_eq!(index.get_action(&action(1)), Ok(None));
    assert_eq!(index.pack(&pack.key), Ok(None));
    assert_eq!(index.locate(&[digest(1)]), Ok(vec![None]));
    admin
        .batch_execute(&format!("DROP TRIGGER refuse ON {schema}.action_result"))
        .expect("removes the trigger");
    let put = index
        .put_action(&action(1), b"r", std::slice::from_ref(&pack))
        .expect("goes through once the trigger is gone");
    assert!(!put.replaced);
    assert!(put.packs[0].created);
}

/// Asserts that a statement the action table refuses for a reason other than a missing table
/// is the store failing, not `NotMigrated` (which tells an operator to run the installer, and
/// would be the wrong advice here), for a read and for a put, and that the put records none of
/// its packs.
#[test]
fn a_table_that_fails_a_statement_is_a_store_failure_not_a_missing_migration() {
    let backend = backend();
    let config = backend.fresh_config();
    let index = PostgresPackIndex::open(&config).expect("opens");
    let schema = config.schema();
    let mut admin = Client::connect(&url(), NoTls).expect("connects");
    admin
        .batch_execute(&format!(
            "ALTER TABLE {schema}.action_result RENAME COLUMN result TO renamed"
        ))
        .expect("a table that lacks the column the statements name");
    let read = index.get_action(&action(1));
    assert!(matches!(read, Err(PackIndexError::Store(_))), "{read:?}");
    let pack = one_blob("cas/d/n1/1/1", 1);
    let put = index.put_action(&action(1), b"r", std::slice::from_ref(&pack));
    assert!(matches!(put, Err(PackIndexError::Store(_))), "{put:?}");
    assert_eq!(
        index.pack(&pack.key),
        Ok(None),
        "the put's pack is not kept"
    );
}

/// Asserts that a schema at migration 2 (the pack tables, no action table) still opens for the
/// pack index and records packs, that the action calls on it are `NotMigrated` with the line
/// that says what to run, that a put records none of its packs and creates nothing, and that
/// both work once the installer's migration has run: a release that carries the action cache
/// adds to a database and does not break what was there.
#[test]
fn a_schema_without_the_action_table_is_refused_until_the_installer_has_run() {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    schema::apply(&config, &pack_schema::embedded(), pack_schema::VERSION)
        .expect("the release before the action cache");
    let index = PostgresPackIndex::open(&config).expect("opens without the action table");
    let kept = one_blob("cas/d/n1/1/1", 1);
    assert!(index.record_pack(&kept).expect("pack calls work").created);
    assert_eq!(
        index.get_action(&action(1)),
        Err(PackIndexError::NotMigrated)
    );
    let pack = one_blob("cas/d/n1/1/2", 2);
    let refused = index.put_action(&action(1), b"r", std::slice::from_ref(&pack));
    let refused = refused.expect_err("no table");
    assert_eq!(refused, PackIndexError::NotMigrated);
    assert!(refused.to_string().contains("kbf-installer setup"));
    assert_eq!(
        index.pack(&pack.key),
        Ok(None),
        "the put's pack is not kept"
    );
    assert!(index.pack(&kept.key).expect("read").is_some());
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let sql = "SELECT to_regclass('kbf_state.action_result')::text";
    let found: Option<String> = client.query_one(sql, &[]).expect("catalog").get(0);
    assert_eq!(found, None);
    schema::apply(&config, &pack_schema::embedded(), pack_schema::latest())
        .expect("the installer's step");
    let put = index
        .put_action(&action(1), b"r", std::slice::from_ref(&pack))
        .expect("the same index goes on");
    assert!(put.packs[0].created);
    assert_eq!(
        index
            .get_action(&action(1))
            .expect("read")
            .map(|e| e.result),
        Some(b"r".to_vec())
    );
}

/// Asserts that with no clock injected the row carries the database's own clock: the time of a
/// put and of the replacing put lie between readings of that clock.
#[test]
fn rows_carry_the_database_clock_when_no_clock_is_injected() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let before = database_now(&mut client);
    index.put_action(&action(1), b"a", &[]).expect("put");
    let first = index.get_action(&action(1)).expect("read").expect("a row");
    index.put_action(&action(1), b"b", &[]).expect("put");
    let second = index.get_action(&action(1)).expect("read").expect("a row");
    let after = database_now(&mut client);
    assert!(before <= first.written_us);
    assert!(first.written_us <= second.written_us);
    assert!(second.written_us <= after);
}

/// Asserts that the table is the one the cache is specified by: the columns and their types, a
/// primary key of hash and size, the result stored inline in the one row of the action, and no
/// foreign key to any other table, so the row refers to nothing in the key/value tables, the
/// feed, the head or the pack tables.
#[test]
fn the_table_has_the_columns_and_key_of_the_cache_and_one_row_per_action() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let sql = "SELECT column_name::text, data_type::text FROM information_schema.columns \
               WHERE table_schema = 'kbf_state' AND table_name = 'action_result' \
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
        ("result", "bytea"),
        ("written_us", "bigint"),
    ];
    let want: Vec<(String, String)> = want
        .iter()
        .map(|(c, t)| ((*c).to_owned(), (*t).to_owned()))
        .collect();
    assert_eq!(columns, want);
    let sql = "SELECT indexdef FROM pg_indexes \
               WHERE schemaname = 'kbf_state' AND tablename = 'action_result'";
    let keys: Vec<String> = client
        .query(sql, &[])
        .expect("catalog")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(
        keys[0].contains("UNIQUE INDEX action_result_pkey"),
        "{keys:?}"
    );
    assert!(keys[0].contains("(hash, size)"), "{keys:?}");
    let sql = "SELECT count(*) FROM pg_constraint \
               WHERE contype = 'f' AND conrelid = 'kbf_state.action_result'::regclass";
    let foreign: i64 = client.query_one(sql, &[]).expect("catalog").get(0);
    assert_eq!(foreign, 0);
    for result in [&b"first"[..], b"second"] {
        index.put_action(&action(1), result, &[]).expect("put");
    }
    let sql = "SELECT result FROM kbf_state.action_result WHERE hash = $1 AND size = $2";
    let rows = client
        .query(sql, &[&action(1).hash.to_vec(), &77_i64])
        .expect("the row");
    assert_eq!(rows.len(), 1, "one row per action digest");
    let inline: Vec<u8> = rows[0].get(0);
    assert_eq!(inline, b"second");
    let read: Option<ActionEntry> = index.get_action(&action(1)).expect("read");
    assert_eq!(read.map(|e| e.result), Some(b"second".to_vec()));
}
