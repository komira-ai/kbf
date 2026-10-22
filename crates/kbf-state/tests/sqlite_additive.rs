//! The SQLite state file changes by expansion only, and a binary skips what it does not
//! know: a file written by a newer schema that only adds a table or a column opens, a file
//! that lacks something this binary needs does not, and a feed row of a kind this binary
//! does not know is left out of the feed.
//!
//! A newer file is made by opening a store, closing it, and applying what a newer release
//! would have added to the file with plain SQL.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{open_manual, scratch};
use kbf_state::sqlite::{Options, SqliteError, SqliteStore, backup};
use kbf_state::{Change, Clock, Cond, Event, Revision, Store, SystemClock, Txn};
use kbf_testkit::dir::TestDir;
use rusqlite::Connection;

/// Writes `value` at `key`, unconditionally.
fn put(store: &SqliteStore, key: &str, value: &str) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// Runs `sql` on the state file `path`, which no store has open.
fn run(path: &Path, sql: &str) {
    let conn = Connection::open(path).expect("open");
    conn.execute_batch(sql).expect("runs");
}

/// The `user_version` the state file `path` records.
fn user_version(path: &Path) -> i64 {
    let conn = Connection::open(path).expect("open");
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("version")
}

/// The `note` column of the only row of `kv` in the state file `path`.
fn note(path: &Path) -> String {
    let conn = Connection::open(path).expect("open");
    conn.query_row("SELECT note FROM kv", [], |row| row.get(0))
        .expect("note")
}

/// Opens the state file in `dir` on the system clock.
fn try_open(dir: &Path) -> Result<SqliteStore, SqliteError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::default());
    SqliteStore::open(&dir.join("state.db"), clock, Options::default())
}

/// A directory whose state file holds the key `k` with the value `v1`, then has `change`
/// applied and its schema version set to 2, as a newer release would leave it.
fn newer_file(name: &str, change: &str) -> TestDir {
    let dir = scratch(name);
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "k", "v1");
    drop(store);
    let file = dir.join("state.db");
    run(&file, change);
    run(&file, "PRAGMA user_version = 2");
    dir
}

/// The feed row `revision` that records `event` on `key`.
fn change(revision: u64, key: &str, event: Event) -> Change {
    Change {
        revision: Revision(revision),
        key: key.to_owned(),
        event,
    }
}

/// Asserts that a state file written by a newer schema, with a column and a table this
/// binary does not know, opens, returns what it holds, takes a write, keeps the newer
/// schema version, and can be backed up.
#[test]
fn a_file_of_a_newer_schema_that_only_adds_opens_and_reads() {
    let dir = newer_file("adds", "ALTER TABLE kv ADD COLUMN note TEXT");
    let file = dir.join("state.db");
    run(&file, "ALTER TABLE feed ADD COLUMN origin INTEGER");
    run(&file, "CREATE TABLE audit (at INTEGER, what TEXT)");
    let store = try_open(&dir).expect("a schema that only adds opens");
    let found = store.get("k").expect("get").expect("k is there");
    assert_eq!(found.value, b"v1".to_vec());
    put(&store, "k2", "v2");
    let feed = store.changes(Revision::ZERO, 10).expect("changes");
    assert_eq!(feed.rows.len(), 2);
    assert_eq!(user_version(&file), 2);
    let summary = backup(&file, &dir.join("copy.db")).expect("a backup of it");
    assert_eq!(summary.keys, 2);
}

/// Asserts that a state file of a newer schema that lacks a column this binary needs is
/// refused with the column named, and its schema version is left alone.
#[test]
fn a_file_of_a_newer_schema_that_lacks_a_needed_column_is_refused() {
    let dir = newer_file("lacks-column", "ALTER TABLE feed DROP COLUMN value");
    let refused = try_open(&dir).expect_err("the feed has no value column");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
    let text = refused.to_string();
    assert!(text.contains("feed.value"), "got {text}");
    assert_eq!(user_version(&dir.join("state.db")), 2);
}

/// Asserts that a state file of a newer schema that lacks a table this binary needs is
/// refused with the table named.
#[test]
fn a_file_of_a_newer_schema_that_lacks_a_needed_table_is_refused() {
    let dir = newer_file("lacks-table", "DROP TABLE counter");
    let refused = try_open(&dir).expect_err("there is no counter table");
    assert!(
        matches!(refused, SqliteError::NotAStateStore(_)),
        "got {refused}"
    );
    let text = refused.to_string();
    assert!(text.contains("counter.name"), "got {text}");
}

/// Asserts that overwriting a key in a state file of a newer schema stores the new value
/// and leaves a column this binary does not know as the newer release set it.
#[test]
fn an_overwrite_keeps_the_columns_it_does_not_know() {
    let dir = newer_file("keeps-column", "ALTER TABLE kv ADD COLUMN note TEXT");
    let file = dir.join("state.db");
    run(&file, "UPDATE kv SET note = 'kept' WHERE key = 'k'");
    let store = try_open(&dir).expect("a schema that only adds opens");
    put(&store, "k", "v2");
    let found = store.get("k").expect("get").expect("k is there");
    assert_eq!(found.value, b"v2".to_vec());
    drop(store);
    assert_eq!(note(&file), "kept");
}

/// Asserts that a feed row of a kind this binary does not know is left out of the page
/// rather than read as an expiry, even when its value is not a blob, that the cursor
/// passes it when it ends a full page (so a poller moves on), and that the rows of the
/// known kinds around it are all returned.
#[test]
fn a_feed_row_of_an_unknown_kind_is_skipped_and_the_known_kinds_still_apply() {
    let dir = scratch("unknown-kind");
    let (store, _) = open_manual(&dir, Options::default());
    put(&store, "a", "v1");
    drop(store);
    let file = dir.join("state.db");
    let inject = |sql: &str| run(&file, sql);
    inject("INSERT INTO feed (revision, key, kind) VALUES (2, 'x', 9)");
    inject("INSERT INTO feed VALUES (3, 'b', 0, X'77')");
    inject("INSERT INTO feed (revision, key, kind) VALUES (4, 'c', 1)");
    inject("INSERT INTO feed (revision, key, kind) VALUES (5, 'd', 2)");
    inject("INSERT INTO feed (revision, key, kind, value) VALUES (6, 'y', 7, 'text')");
    inject("UPDATE meta SET value = 6 WHERE name = 'head'");
    let (store, _) = open_manual(&dir, Options::default());

    let all = store.changes(Revision::ZERO, 10).expect("changes");
    assert_eq!(
        all.rows,
        [
            change(1, "a", Event::Put(b"v1".to_vec())),
            change(3, "b", Event::Put(b"w".to_vec())),
            change(4, "c", Event::Delete),
            change(5, "d", Event::Expire),
        ]
    );
    assert_eq!(all.cursor, Revision(6));

    let first = store.changes(Revision::ZERO, 2).expect("changes");
    assert_eq!((first.rows.len(), first.cursor), (1, Revision(2)));
    let middle = store.changes(Revision(1), 1).expect("changes");
    assert_eq!((middle.rows.len(), middle.cursor), (0, Revision(2)));
    let tail = store.changes(Revision(5), 1).expect("changes");
    assert_eq!((tail.rows.len(), tail.cursor), (0, Revision(6)));
}
