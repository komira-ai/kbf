//! The SQLite state file as the tool reads it: what it holds, in the units of the Postgres
//! store, and what it refuses to read.

mod support;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use kbf_state_copy::Error;
use kbf_state_copy::digest::Entry;
use kbf_state_copy::lock;
use kbf_state_copy::source::{self, Snapshot};
use rusqlite::Connection;
use support::{
    GOLDEN_FAMILIES, GOLDEN_HEAD, GOLDEN_HIGHEST, built, golden_copy, golden_rows, lower_head,
    names, scratch,
};

/// Reads the state file `db` by its canonical name, as a run does.
fn read(db: &Path) -> Result<Snapshot, Error> {
    source::read(&lock::canonical(db)?)
}

fn row<'a>(snapshot: &'a Snapshot, key: &str) -> &'a Entry {
    snapshot
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .expect("the row is there")
}

/// A new SQLite file with a table and the given `user_version`, named for that version so a
/// test can make one file per version in the same directory.
fn file_with_version(dir: &Path, version: i64) -> PathBuf {
    let db = dir.join(format!("other-{version}.db"));
    let conn = Connection::open(&db).expect("create the file");
    conn.execute_batch(&format!(
        "CREATE TABLE t (x); PRAGMA user_version = {version}"
    ))
    .expect("set the version");
    db
}

/// Asserts that the golden file reads as the rows its `.rows` file lists, with each expiry
/// in microseconds, the head revision its feed ends at, its one counter, and the families
/// and row counts of the file.
#[test]
fn the_golden_file_reads_as_the_rows_it_is_recorded_to_hold() {
    let dir = scratch("source-golden");
    let snapshot = read(&golden_copy(&dir)).expect("the golden file reads");
    assert_eq!(snapshot.entries, golden_rows());
    assert_eq!(snapshot.head, GOLDEN_HEAD);
    assert_eq!(snapshot.highest_version(), GOLDEN_HIGHEST);
    assert_eq!(snapshot.target_head(), GOLDEN_HEAD);
    let counters: Vec<(&str, i64)> = snapshot
        .counters
        .iter()
        .map(|counter| (counter.name.as_str(), counter.value))
        .collect();
    assert_eq!(counters, [("golden/counter", 2)]);
    let summary = snapshot.summary();
    let families: Vec<(&str, u64)> = summary
        .keys
        .iter()
        .map(|family| (family.name.as_str(), family.row_count))
        .collect();
    assert_eq!(families, GOLDEN_FAMILIES);
    let done = row(&snapshot, "done/0000000000000001");
    assert_eq!(done.expires_us, Some(600_000_000));
    let session = row(&snapshot, "session/node-a");
    assert_eq!(session.expires_us, Some(60_000_000));
    assert_eq!(row(&snapshot, "cordon/node-a").expires_us, None);
}

/// Asserts that a file written through the SQLite store reads back row for row: a row
/// written twice with its later version, binary and empty values, a key with no slash and
/// one with a non-ASCII family, an expiry of 90 s in microseconds and one of 1500 ns rounded
/// down, no row for a deleted key, the counters, and the head revision.
#[test]
fn a_file_the_sqlite_store_wrote_reads_back_row_for_row() {
    let dir = scratch("source-built");
    let written = built(&dir);
    let snapshot = read(&written.db).expect("the file reads");
    assert_eq!(snapshot.entries, written.entries);
    assert_eq!(snapshot.counters, written.counters);
    assert_eq!(snapshot.head, written.head);
    assert_eq!(row(&snapshot, "member/n1").expires_us, Some(90_000_000));
    assert_eq!(row(&snapshot, "member/n2").expires_us, Some(1));
    assert_eq!(row(&snapshot, "lease/a").value, [0, 255, 128]);
    assert!(row(&snapshot, "queue/0001").value.is_empty());
    assert_eq!(row(&snapshot, "op/0001").value, b"running");
    assert!(snapshot.entries.iter().all(|entry| entry.key != "gone/1"));
    assert_eq!(snapshot.counters.len(), 2);
}

/// Asserts that the head the target is left at is the highest version when the file records
/// a head below it, and the recorded head otherwise.
#[test]
fn the_target_head_is_never_below_the_highest_version() {
    let dir = scratch("source-lower-head");
    let db = golden_copy(&dir);
    lower_head(&db);
    let snapshot = read(&db).expect("the copy reads");
    assert_eq!(snapshot.head, 0);
    assert_eq!(snapshot.target_head(), GOLDEN_HIGHEST);
}

/// Asserts that reading a state file leaves nothing behind: its bytes are the same and no
/// file appears beside it, not a log, not a shared-memory file, not a journal.
#[test]
fn reading_a_file_leaves_no_trace_beside_it() {
    let dir = scratch("source-no-trace");
    let db = golden_copy(&dir);
    let bytes = fs::read(&db).expect("read the copy");
    let before = names(&dir);
    let first = read(&db).expect("the first read");
    let second = read(&db).expect("the second read");
    assert_eq!(first.entries, second.entries);
    assert_eq!(fs::read(&db).expect("read again"), bytes);
    assert_eq!(names(&dir), before);
}

/// Asserts that a file with another `user_version`, or none, is refused with the version it
/// has and the one the tool reads.
#[test]
fn a_file_of_another_layout_is_refused() {
    let dir = scratch("source-version");
    let other = file_with_version(&dir, 2);
    let err = read(&other).expect_err("version 2 is refused");
    assert_eq!(err.code(), 2);
    let text = err.to_string();
    assert!(text.contains("schema version 2"), "{text}");
    assert!(text.contains("reads version 1"), "{text}");
    let none = file_with_version(&dir, 0);
    let err = read(&none).expect_err("version 0 is refused");
    assert_eq!(err.code(), 2);
    assert!(err.to_string().contains("schema version 0"));
}

/// Asserts that a file with the right version and none of the tables fails, naming SQLite's
/// reason, and is not taken for a state store.
#[test]
fn a_file_with_the_right_version_and_the_wrong_tables_fails() {
    let dir = scratch("source-tables");
    let err = read(&file_with_version(&dir, 1)).expect_err("no tables");
    assert_eq!(err.code(), 1);
    assert!(err.to_string().contains("no such table"), "{err}");
}

/// Asserts that a file that is not a database fails with SQLite's reason, and a file that is
/// not there fails too, neither as a refusal.
#[test]
fn a_file_that_is_not_a_database_or_not_there_fails() {
    let dir = scratch("source-garbage");
    let garbage = dir.join("garbage.db");
    fs::write(&garbage, "not a database ".repeat(20)).expect("write the file");
    let err = read(&garbage).expect_err("garbage");
    assert_eq!(err.code(), 1);
    assert!(
        err.to_string().starts_with("failed: the state file: "),
        "{err}"
    );
    let err = source::read(&dir.join("missing.db")).expect_err("missing");
    assert_eq!(err.code(), 1);
    let err = read(&dir.join("missing.db")).expect_err("missing by its canonical name");
    assert_eq!(err.code(), 1);
    assert!(err.to_string().contains("missing.db"), "{err}");
}

/// Asserts that a log beside the file that holds bytes is refused, naming the log and its
/// size (an immutable read would not see those writes), and that an empty log is not.
#[test]
fn a_log_that_holds_writes_is_refused_and_an_empty_one_is_not() {
    let dir = scratch("source-log");
    let db = golden_copy(&dir);
    let log = lock::beside(&db, "-wal");
    fs::write(&log, [7_u8; 32]).expect("write a log");
    let err = read(&db).expect_err("a log with writes");
    assert_eq!(err.code(), 2);
    let text = err.to_string();
    assert!(text.contains("state.db-wal holds 32 bytes"), "{text}");
    fs::write(&log, b"").expect("empty the log");
    let snapshot = read(&db).expect("an empty log is nothing");
    assert_eq!(snapshot.entries, golden_rows());
}

/// Asserts that the URI escapes the characters a URI gives a meaning to, so that a path
/// with a percent sign, a question mark or a hash opens the file it names, and refuses a
/// path that is not UTF-8.
#[test]
fn a_path_with_uri_characters_opens_the_file_it_names() {
    let plain = source::uri(Path::new("/a/state.db")).expect("a plain path");
    assert_eq!(plain, "file:/a/state.db?immutable=1");
    let odd = source::uri(Path::new("/100%/a?b#c.db")).expect("an odd path");
    assert_eq!(odd, "file:/100%25/a%3fb%23c.db?immutable=1");
    let bytes = Path::new(OsStr::from_bytes(b"/tmp/\xff.db"));
    let err = source::uri(bytes).expect_err("not UTF-8");
    assert_eq!(err.code(), 1);
    let dir = scratch("source-odd-path");
    let folder = dir.join("odd 100%?#");
    fs::create_dir(&folder).expect("make the directory");
    let snapshot = read(&golden_copy(&folder)).expect("the file in the odd directory reads");
    assert_eq!(snapshot.entries, golden_rows());
}
