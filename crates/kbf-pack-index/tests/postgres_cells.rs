//! The cell rows and unlisted trees on a real Postgres server: the conformance clauses (the
//! ones `tests/cells_memory.rs` runs against the in-memory index), and what is Postgres's
//! alone: a schema without the tables says so until the installer has run, the rows carry the
//! database's clock when none is injected, and the tables are the ones the rows are specified
//! by.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a database
//! they may create and drop schemas in. No assertion depends on elapsed time.
#![cfg(feature = "postgres")]

use std::time::Duration;

use kbf_pack_index::cells::CELL_BYTES;
use kbf_pack_index::cells_conformance::{
    c1_recorded_cells_are_read_back, c2_the_same_cells_again_change_nothing,
    c3_other_cells_conflict_and_change_nothing, c4_cells_that_cannot_exist_are_refused,
    c5_the_size_boundaries, c6_a_read_of_keys_is_bounded, c7_a_key_must_not_be_empty,
    c8_cells_are_kept_apart_from_packs, cells, u1_an_unlisted_tree_is_recorded_and_moved,
    u2_unlisted_trees_come_by_id_after_a_cursor, u3_the_window_edge_is_listed_and_older_is_not,
    u4_a_page_is_bounded, u5_a_tree_is_dropped_once,
    u6_live_packs_skip_dead_ones_and_say_whether_they_have_cells,
};
use kbf_pack_index::postgres::{PostgresPackBackend, PostgresPackIndex};
use kbf_pack_index::schema::{self as pack_schema, test_database};
use kbf_pack_index::{PackIndex, PackIndexError};
use kbf_state::postgres::schema;
use kbf_state::postgres::{PostgresConfig, TestDatabase};
use postgres::{Client, NoTls};

fn backend() -> PostgresPackBackend {
    PostgresPackBackend::new(&kbf_testkit::database::server_url())
}

// ---- the suite -------------------------------------------------------------------------

/// Asserts that recorded cells are read back whole, in the order asked, with none for a key
/// never recorded.
#[test]
fn c1_recorded_cells_are_read_back_in_order() {
    c1_recorded_cells_are_read_back(&backend());
}

/// Asserts that the same cells recorded again answer false and change nothing.
#[test]
fn c2_the_same_cells_twice_change_nothing() {
    c2_the_same_cells_again_change_nothing(&backend());
}

/// Asserts that other cells under a recorded key are a conflict and the first row is kept.
#[test]
fn c3_other_cells_are_a_conflict_that_keeps_the_first_row() {
    c3_other_cells_conflict_and_change_nothing(&backend());
}

/// Asserts that no bytes, a size over a bigint and a wrong digest count are refused with
/// nothing written.
#[test]
fn c4_cells_that_cannot_exist_write_nothing() {
    c4_cells_that_cannot_exist_are_refused(&backend());
}

/// Asserts the digest counts at the cell boundaries and the refusal of the largest size.
#[test]
fn c5_the_sizes_at_the_cell_boundaries() {
    c5_the_size_boundaries(&backend());
}

/// Asserts the batch bound of a read, at the bound and one past it.
#[test]
fn c6_a_read_is_bounded() {
    c6_a_read_of_keys_is_bounded(&backend());
}

/// Asserts that an empty key is refused.
#[test]
fn c7_an_empty_key_is_refused() {
    c7_a_key_must_not_be_empty(&backend());
}

/// Asserts that cells and packs under one key do not make each other.
#[test]
fn c8_cells_and_packs_are_kept_apart() {
    c8_cells_are_kept_apart_from_packs(&backend());
}

/// Asserts that an unlisted tree has the clock as first and last time and that recording it
/// again moves only the last.
#[test]
fn u1_an_unlisted_tree_keeps_its_first_time() {
    u1_an_unlisted_tree_is_recorded_and_moved(&backend());
}

/// Asserts the byte order of trees, the exclusive cursor and the limit.
#[test]
fn u2_unlisted_trees_are_paged_by_id() {
    u2_unlisted_trees_come_by_id_after_a_cursor(&backend());
}

/// Asserts that a tree exactly at the window edge is listed and one a microsecond older is not.
#[test]
fn u3_the_window_edge_is_inclusive() {
    u3_the_window_edge_is_listed_and_older_is_not(&backend());
}

/// Asserts the page bound for trees and packs, at the bound and one past it.
#[test]
fn u4_a_page_is_bounded_for_trees_and_packs() {
    u4_a_page_is_bounded(&backend());
}

/// Asserts that a drop answers true once and false after, and removes only that tree.
#[test]
fn u5_a_tree_drops_once() {
    u5_a_tree_is_dropped_once(&backend());
}

/// Asserts the live-pack sweep: dead and empty packs skipped, byte order of keys whatever the
/// database's collation, the exclusive cursor and the cells flag.
#[test]
fn u6_live_packs_are_swept_in_byte_order_with_their_cells_flag() {
    u6_live_packs_skip_dead_ones_and_say_whether_they_have_cells(&backend());
}

// ---- Postgres alone ----------------------------------------------------------------------

/// Asserts that a schema at migration 4 (no cell tables) still opens for the pack index, that
/// each of the six calls on it is `NotMigrated` with the line that says what to run, and that
/// the calls work on the same index once the installer's migration has run.
#[test]
fn a_schema_without_the_cell_tables_is_refused_until_the_installer_has_run() {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    schema::apply(
        &config,
        &pack_schema::embedded(),
        pack_schema::DIRECTORY_VERSION,
    )
    .expect("the release before the L2 cells");
    let index = PostgresPackIndex::open(&config).expect("opens without the cell tables");
    let row = cells(5, 1);
    let tree = [1; 32];
    let refused = [
        index.object_cells(&["k".to_owned()]).map(|_| ()),
        index.record_cells("k", &row).map(|_| ()),
        index.record_unlisted(&tree),
        index.unlisted(Duration::from_secs(1), None, 1).map(|_| ()),
        index.drop_unlisted(&tree).map(|_| ()),
        index.live_packs(None, 1).map(|_| ()),
    ];
    for answer in refused {
        assert_eq!(answer, Err(PackIndexError::NotMigrated));
    }
    let said = index.record_cells("k", &row).expect_err("no table");
    assert!(said.to_string().contains("kbf-installer setup"));
    schema::apply(&config, &pack_schema::embedded(), pack_schema::latest())
        .expect("the installer's step");
    assert_eq!(index.record_cells("k", &row), Ok(true));
    assert_eq!(index.object_cells(&["k".to_owned()]), Ok(vec![Some(row)]));
}

/// Asserts that with no clock injected a tree carries the database's own clock as its first
/// and last time.
#[test]
fn unlisted_trees_carry_the_database_clock_when_no_clock_is_injected() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let now = "SELECT floor(extract(epoch from clock_timestamp()) * 1000000)::bigint";
    let before: i64 = client.query_one(now, &[]).expect("the clock").get(0);
    index.record_unlisted(&[7; 32]).expect("record");
    let after: i64 = client.query_one(now, &[]).expect("the clock").get(0);
    let sql = "SELECT first_us, last_us FROM kbf_state.unlisted_tree";
    let row = client.query_one(sql, &[]).expect("the row");
    let (first, last): (i64, i64) = (row.get(0), row.get(1));
    assert!(before <= first && first <= last && last <= after);
    let listed = index.unlisted(Duration::from_secs(3600), None, 10);
    assert_eq!(listed.map(|l| l.len()), Ok(1));
}

/// Asserts that the tables are the ones the rows are specified by: the columns and their
/// types, a primary key of the key and of the tree, and the digests stored as the 32-byte
/// digests concatenated.
#[test]
fn the_tables_have_the_columns_and_keys_of_the_cell_rows() {
    let db = test_database();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    let index = PostgresPackIndex::open(&config).expect("opens");
    let mut client = Client::connect(&db.url(), NoTls).expect("connects");
    let columns = |client: &mut Client, table: &str| -> Vec<(String, String)> {
        let sql = "SELECT column_name::text, data_type::text FROM information_schema.columns \
                   WHERE table_schema = 'kbf_state' AND table_name = $1 \
                   ORDER BY ordinal_position";
        let rows = client.query(sql, &[&table]).expect("the catalog");
        rows.iter().map(|r| (r.get(0), r.get(1))).collect()
    };
    let named = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(c, t)| ((*c).to_owned(), (*t).to_owned()))
            .collect()
    };
    assert_eq!(
        columns(&mut client, "object_cells"),
        named(&[("key", "text"), ("size", "bigint"), ("digests", "bytea")])
    );
    assert_eq!(
        columns(&mut client, "unlisted_tree"),
        named(&[
            ("tree", "bytea"),
            ("first_us", "bigint"),
            ("last_us", "bigint")
        ])
    );
    let sql = "SELECT tablename::text, indexdef FROM pg_indexes \
               WHERE schemaname = 'kbf_state' AND tablename IN ('object_cells', 'unlisted_tree') \
               ORDER BY tablename";
    let defs: Vec<(String, String)> = client
        .query(sql, &[])
        .expect("the catalog")
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(defs.len(), 2, "{defs:?}");
    assert!(
        defs[0].1.contains("UNIQUE") && defs[0].1.contains("(key)"),
        "{defs:?}"
    );
    assert!(
        defs[1].1.contains("UNIQUE") && defs[1].1.contains("(tree)"),
        "{defs:?}"
    );
    let row = cells(2 * CELL_BYTES + 1, 5);
    index.record_cells("cas/f/x", &row).expect("record");
    let sql = "SELECT size, digests FROM kbf_state.object_cells WHERE key = 'cas/f/x'";
    let held = client.query_one(sql, &[]).expect("the row");
    let (size, digests): (i64, Vec<u8>) = (held.get(0), held.get(1));
    assert_eq!(size, 2 * i64::try_from(CELL_BYTES).expect("small") + 1);
    assert_eq!(digests, row.digests.concat());
    assert_eq!(digests.len(), 96);
}
