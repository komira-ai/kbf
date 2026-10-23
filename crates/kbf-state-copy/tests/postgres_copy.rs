//! The copy against a real Postgres server: what it writes, what it refuses, and what it
//! does when the copy does not read back the same.
//!
//! The tests are `#[ignore]`d so `cargo test --workspace` stays hermetic; the Postgres jobs
//! of CI run them with `--include-ignored` against a service container.
//! `KBF_TEST_POSTGRES_URL` names a server on which they may make and drop databases. The
//! store lives in the default schema of the URL it is given, so each test makes a database
//! of its own: nextest runs every test in a process of its own, side by side.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use kbf_state::postgres::{PostgresConfig, PostgresStore};
use kbf_state::{Cond, Revision, Store, Txn, Version};
use kbf_state_copy::source;
use kbf_state_copy::{Error, Seams, Stage, lock};
use support::{
    Database, GOLDEN_FAMILIES, GOLDEN_HEAD, GOLDEN_HIGHEST, args, built, bulk, count, counter,
    golden_copy, golden_rows, has, head, lacks, lower_head, names, open, pg_counters, pg_entries,
    run_with, scratch, settle, shape, starts,
};

type Tamper = fn(Stage) -> Option<&'static str>;

fn dry(db: &Path, url: &str) -> (Result<(), Error>, String) {
    run_with(&args(db, url, false), &Seams::default())
}

fn apply(db: &Path, url: &str) -> (Result<(), Error>, String) {
    run_with(&args(db, url, true), &Seams::default())
}

/// Copies `db` into the target at `url`, checks that the copy was accepted, and returns the
/// report.
fn copied(db: &Path, url: &str) -> String {
    let (result, report) = apply(db, url);
    result.expect("the copy is accepted");
    report
}

fn with_tamper(tamper: Tamper) -> Seams {
    Seams {
        tamper,
        ..Seams::default()
    }
}

fn open_store(database: &Database) -> PostgresStore {
    let config = PostgresConfig::parse(&database.url()).expect("the URL parses");
    PostgresStore::open(&config).expect("the store opens")
}

/// Writes `key` through the store and returns the revision it was given.
fn put(store: &PostgresStore, key: &str) -> Revision {
    let commit = store
        .txn(Txn::new().put(key, "v", Cond::Any))
        .expect("the store works")
        .expect("the put commits");
    commit.revision
}

/// The next value of the schema's revision sequence.
fn next_sequence(client: &mut postgres::Client) -> i64 {
    let sql = "SELECT nextval('kbf_state.rev_seq')";
    client.query_one(sql, &[]).expect("the sequence").get(0)
}

/// Asserts that a dry run of the golden file against an empty target at the current schema
/// is accepted, reports every family with its row count, and writes nothing: not to the
/// target, not to the state file, not beside it.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_dry_run_reports_every_family_and_writes_nothing() {
    let dir = scratch("copy-dry-run");
    let db = golden_copy(&dir);
    let bytes = fs::read(&db).expect("read the file");
    let before = names(&dir);
    let target = Database::with_schema();
    let (result, report) = dry(&db, &target.url());
    result.expect("a dry run against a clean target is accepted");
    for (family, rows) in GOLDEN_FAMILIES {
        has(&report, &format!("  {family:<8}  {rows:>6}  "));
    }
    has(
        &report,
        "key/value rows: 14; counters: 1; highest version: 36; head revision: 36",
    );
    has(&report, "the target is empty, its schema is current");
    has(&report, "dry run: nothing was written");
    lacks(&report, "copied");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(fs::read(&db).expect("read again"), bytes);
    assert_eq!(names(&dir), before);
}

/// Asserts that a copy of the golden file puts every row, counter, version and expiry in the
/// target as the file's own record lists them (expiry in microseconds), copies no change
/// feed, sets the head to the file's head, leaves the state file as it was, and reports each
/// family as matching.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_copy_keeps_every_row_counter_version_and_expiry() {
    let dir = scratch("copy-golden");
    let db = golden_copy(&dir);
    let bytes = fs::read(&db).expect("read the file");
    let target = Database::with_schema();
    let report = copied(&db, &target.url());
    let mut client = target.client();
    assert_eq!(pg_entries(&mut client), golden_rows());
    assert_eq!(pg_counters(&mut client), [counter("golden/counter", 2)]);
    assert_eq!(count(&mut client, "feed"), 0);
    assert_eq!(head(&mut client), GOLDEN_HEAD);
    has(
        &report,
        "copied key/value rows: 14, counters: 1; head revision now 36\n",
    );
    has(&report, "10 families match");
    assert_eq!(fs::read(&db).expect("read again"), bytes);
}

/// Asserts that a file with every kind of row the store can hold (a row written twice, a
/// binary value, an empty value, a key with no slash, a non-ASCII family, an expiry that is
/// not a whole number of microseconds, counters) copies row for row what was written to it.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_copy_of_a_file_with_every_kind_of_row_matches_what_was_written() {
    let dir = scratch("copy-built");
    let written = built(&dir);
    let target = Database::with_schema();
    copied(&written.db, &target.url());
    let mut client = target.client();
    assert_eq!(pg_entries(&mut client), written.entries);
    assert_eq!(pg_counters(&mut client), written.counters);
    assert_eq!(head(&mut client), written.head);
}

/// Asserts that after a copy a write gets the revision after the file's head, which is above
/// every copied version; that a compare-and-set on the version the file held for a row
/// succeeds and one on another version is a conflict on that key; that a counter goes on
/// from the value the file held; and that the schema's revision sequence is past the head.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn writes_after_the_copy_continue_from_the_copied_versions() {
    let dir = scratch("copy-continue");
    let db = golden_copy(&dir);
    let target = Database::with_schema();
    copied(&db, &target.url());
    let store = open_store(&target);
    let lease = "lease/01a14bc2ee4ea98f.0000000000000003";
    let stale = Txn::new().put(lease, "taken", Cond::Version(Version(34)));
    let conflict = store
        .txn(stale)
        .expect("the store works")
        .expect_err("a version the file did not hold");
    assert_eq!(conflict.key, lease);
    let held = Txn::new().put(lease, "taken", Cond::Version(Version(35)));
    let commit = store
        .txn(held)
        .expect("the store works")
        .expect("the version the file held");
    assert_eq!(commit.revision, Revision(37));
    assert!(commit.revision.0 > u64::try_from(GOLDEN_HIGHEST).expect("small"));
    assert_eq!(put(&store, "new/row"), Revision(38));
    let counted = store
        .txn(Txn::new().next("golden/counter"))
        .expect("the store works")
        .expect("the counter commits");
    assert_eq!(counted.counters, [3]);
    assert_eq!(next_sequence(&mut target.client()), GOLDEN_HEAD + 1);
}

/// Asserts that when the file records a head below its highest version, the target's head is
/// the highest version and the first write is the revision after it.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn the_head_is_the_highest_version_when_the_file_records_a_lower_head() {
    let dir = scratch("copy-lower-head");
    let db = golden_copy(&dir);
    lower_head(&db);
    let target = Database::with_schema();
    copied(&db, &target.url());
    assert_eq!(head(&mut target.client()), GOLDEN_HIGHEST);
    let revision = put(&open_store(&target), "new/row");
    assert_eq!(revision, Revision(37));
    assert_eq!(next_sequence(&mut target.client()), GOLDEN_HIGHEST + 1);
}

/// Asserts that a state file with nothing in it copies, leaves the head at 0, and that the
/// first write to the target is revision 1.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_state_file_with_nothing_in_it_copies_and_the_first_write_is_revision_1() {
    let dir = scratch("copy-empty");
    let db = dir.join("state.db");
    drop(open(&db));
    settle(&db);
    let target = Database::with_schema();
    let report = copied(&db, &target.url());
    has(
        &report,
        "copied key/value rows: 0, counters: 0; head revision now 0\n",
    );
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(put(&open_store(&target), "first/row"), Revision(1));
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a copy of more rows and counters than one INSERT carries is complete and
/// matches the file row for row.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_copy_of_more_rows_than_one_insert_carries_is_complete() {
    let dir = scratch("copy-bulk");
    let db = bulk(&dir, 1_203, 601);
    let target = Database::with_schema();
    let report = copied(&db, &target.url());
    has(&report, "copied key/value rows: 1203, counters: 601;");
    let mut client = target.client();
    assert_eq!(count(&mut client, "kv"), 1_203);
    assert_eq!(count(&mut client, "counters"), 601);
    let name = lock::canonical(&db).expect("the file is there");
    let snapshot = source::read(&name).expect("the file reads");
    assert_eq!(pg_entries(&mut client), snapshot.entries);
    assert_eq!(pg_counters(&mut client), snapshot.counters);
}

/// Asserts that a target that holds anything (a row, only the feed and head a deleted row
/// leaves, only a counter) is refused with status 2, and that it is as it was afterwards.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_target_that_holds_anything_is_refused_and_left_as_it_was() {
    let dir = scratch("copy-not-empty");
    let db = golden_copy(&dir);
    let with_row = Database::with_schema();
    put(&open_store(&with_row), "x/1");
    let emptied = Database::with_schema();
    let store = open_store(&emptied);
    put(&store, "x/1");
    let deleted = store.txn(Txn::new().delete("x/1", Cond::Any));
    deleted.expect("the store works").expect("a delete");
    let counted = Database::with_schema();
    let bumped = open_store(&counted).txn(Txn::new().next("c"));
    bumped.expect("the store works").expect("a count");
    let cases = [
        (&with_row, [1, 1, 0, 1]),
        (&emptied, [0, 2, 0, 2]),
        (&counted, [0, 0, 1, 0]),
    ];
    for (target, held) in cases {
        assert_eq!(shape(&mut target.client()), held);
        let (result, report) = apply(&db, &target.url());
        let error = result.expect_err("a store that holds data is refused");
        assert_eq!(error.code(), 2);
        has(&error.to_string(), "already holds data");
        lacks(&report, "copied");
        let after = shape(&mut target.client());
        assert_eq!(after, held, "the store is as it was");
    }
}

/// Asserts that a second copy into a target a copy has filled is refused and changes
/// nothing, with status 2.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_second_copy_is_refused_and_changes_nothing() {
    let dir = scratch("copy-twice");
    let db = golden_copy(&dir);
    let target = Database::with_schema();
    copied(&db, &target.url());
    let rows = pg_entries(&mut target.client());
    let error = apply(&db, &target.url()).0.expect_err("the second copy");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "already holds data");
    assert_eq!(pg_entries(&mut target.client()), rows);
    assert_eq!(count(&mut target.client(), "counters"), 1);
}

/// Asserts that a database with no schema, and one whose schema is a version this tool was
/// not built for, are each refused with status 2 naming the version found, and are left
/// empty; and that the report of the file is printed before the refusal.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_store_whose_schema_is_not_current_is_refused() {
    let dir = scratch("copy-schema");
    let db = golden_copy(&dir);
    let bare = Database::bare();
    let (result, report) = dry(&db, &bare.url());
    let error = result.expect_err("a database with no schema is refused");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "schema is at version 0");
    has(&report, "key families (rows, digest):");
    let ahead = Database::with_schema();
    let sql = "INSERT INTO kbf_state.schema_version (version, name, kind) \
               VALUES (2, 'later', 'expand')";
    ahead
        .client()
        .batch_execute(sql)
        .expect("record a later version");
    let (result, _report) = apply(&db, &ahead.url());
    let error = result.expect_err("a schema the tool does not know is refused");
    assert_eq!(error.code(), 2);
    has(&error.to_string(), "schema is at version 2");
    assert_eq!(shape(&mut ahead.client()), [0, 0, 0, 0]);
}

fn epoch_clock() -> i64 {
    0
}

/// Asserts that a host clock far from the database's is refused with status 2 for a dry run
/// and for a copy, and that nothing is written.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_clock_that_is_off_is_refused_and_nothing_is_written() {
    let dir = scratch("copy-clock");
    let db = golden_copy(&dir);
    let target = Database::with_schema();
    let seams = Seams {
        local_now_us: epoch_clock,
        ..Seams::default()
    };
    for copying in [false, true] {
        let (result, report) = run_with(&args(&db, &target.url(), copying), &seams);
        let error = result.expect_err("a clock that is off is refused");
        assert_eq!(error.code(), 2);
        has(&error.to_string(), "differ by");
        lacks(&report, "copied");
    }
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
}

fn change_a_value(stage: Stage) -> Option<&'static str> {
    let sql = "UPDATE kv SET value = decode('00', 'hex') WHERE key = 'op/0000000000000000'";
    (stage == Stage::InTransaction).then_some(sql)
}

fn remove_a_row(stage: Stage) -> Option<&'static str> {
    let sql = "DELETE FROM kv WHERE key = 'cordon/node-a'";
    (stage == Stage::InTransaction).then_some(sql)
}

fn age_the_counter(stage: Stage) -> Option<&'static str> {
    let sql = "UPDATE counters SET value = value - 1";
    (stage == Stage::InTransaction).then_some(sql)
}

fn add_a_row(stage: Stage) -> Option<&'static str> {
    let sql = "INSERT INTO kv VALUES ('zzz/extra', decode('00', 'hex'), 1, NULL)";
    (stage == Stage::InTransaction).then_some(sql)
}

fn ask_for_a_table_that_is_not_there(stage: Stage) -> Option<&'static str> {
    (stage == Stage::InTransaction).then_some("SELECT * FROM missing_table")
}

fn change_a_version_after_the_commit(stage: Stage) -> Option<&'static str> {
    let sql = "UPDATE kv SET version = version + 1 WHERE key = 'meta/epoch'";
    (stage == Stage::AfterCommit).then_some(sql)
}

fn unset_the_head(stage: Stage) -> Option<&'static str> {
    (stage == Stage::InTransaction).then_some("UPDATE head SET rev = 0")
}

fn ask_for_a_table_that_is_not_there_after_the_commit(stage: Stage) -> Option<&'static str> {
    (stage == Stage::AfterCommit).then_some("SELECT * FROM missing_table")
}

/// Copies the golden file with `tamper` acting on the target, checks that the state file is
/// as it was, and returns the failure and the target.
fn failed_copy(tamper: Tamper) -> (Error, Database) {
    let dir = scratch("copy-tampered");
    let db = golden_copy(&dir);
    let bytes = fs::read(&db).expect("read the file");
    let target = Database::with_schema();
    let arguments = args(&db, &target.url(), true);
    let (result, _report) = run_with(&arguments, &with_tamper(tamper));
    let after = fs::read(&db).expect("read again");
    assert_eq!(after, bytes, "the state file is untouched");
    (result.expect_err("the copy must fail"), target)
}

/// Asserts that a value that is not what the file held, found when the rows are read back
/// inside the transaction, fails the copy with status 1 naming the family, and that the
/// transaction is rolled back: the target is as empty as it was.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_value_that_changed_before_the_check_fails_the_copy_and_rolls_it_back() {
    let (error, target) = failed_copy(change_a_value);
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "key family op/: state file 2 rows");
    has(&said, "state file before the commit");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a row missing when the rows are read back inside the transaction fails the
/// copy naming the family as absent in the copy, and rolls it back.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_row_that_went_missing_before_the_check_fails_the_copy_and_rolls_it_back() {
    let (error, target) = failed_copy(remove_a_row);
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "key family cordon/: state file 1 rows");
    has(&said, ", copy absent");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a counter whose value is not the file's, found inside the transaction, fails
/// the copy naming its family, and rolls it back.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_counter_out_of_date_before_the_check_fails_the_copy_and_rolls_it_back() {
    let (error, target) = failed_copy(age_the_counter);
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "counter family golden/: state file 1 rows");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a row the file does not hold, found inside the transaction, fails the copy
/// naming its family as absent from the file, and rolls it back.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_row_the_file_does_not_hold_fails_the_copy_and_rolls_it_back() {
    let (error, target) = failed_copy(add_a_row);
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "key family zzz/: state file absent");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a statement the server refuses fails the copy with status 1, naming the
/// server's SQLSTATE, and rolls it back.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_statement_the_server_refuses_fails_the_copy_with_its_sqlstate() {
    let (error, target) = failed_copy(ask_for_a_table_that_is_not_there);
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "SQLSTATE 42P01");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a head revision that is not the one the copy set, found when the head is read
/// back inside the transaction, fails the copy with status 1 naming both revisions, and rolls
/// it back: the target is empty and its revision sequence is where it started.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_head_that_is_not_the_copied_one_fails_the_copy_and_rolls_it_back() {
    let (error, target) = failed_copy(unset_the_head);
    assert_eq!(error.code(), 1);
    has(&error.to_string(), "the head revision is 0, not 36");
    has(&error.to_string(), "before the commit");
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    assert_eq!(next_sequence(&mut target.client()), 1);
}

/// Asserts that a read-back after the commit that fails (here, a statement the server
/// refuses) is reported with status 1 as a copy that was committed, naming the server's
/// SQLSTATE, and that the committed copy is left in place.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_read_back_that_fails_after_the_commit_says_the_copy_is_committed() {
    let (error, target) = failed_copy(ask_for_a_table_that_is_not_there_after_the_commit);
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "the copy was committed but could not be read back");
    has(&said, "SQLSTATE 42P01");
    assert_eq!(count(&mut target.client(), "kv"), 14);
}

/// Asserts that a copy that reads back differently on a new connection after the commit is
/// reported as a failure with status 1 saying the target holds a copy that does not match,
/// that the committed copy is left in place, and that the state file is untouched.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn a_copy_that_differs_after_the_commit_is_reported_and_left_in_place() {
    let (error, target) = failed_copy(change_a_version_after_the_commit);
    assert_eq!(error.code(), 1);
    let said = error.to_string();
    has(&said, "key family meta/");
    has(
        &said,
        "after the commit; the target holds a copy that does not match",
    );
    assert_eq!(count(&mut target.client(), "kv"), 14);
}

static WATCHED: OnceLock<PathBuf> = OnceLock::new();
static SEEN: AtomicU8 = AtomicU8::new(0);

/// Tries to take the frontend lock from inside the copy's transaction, and notes the exit
/// status of the answer: 2 for a refusal.
fn try_the_lock(stage: Stage) -> Option<&'static str> {
    let db = WATCHED.get().expect("the path is set before the run");
    if stage == Stage::InTransaction {
        let status = lock::take(db).err().map_or(0, |error| error.code());
        SEEN.store(status, Ordering::SeqCst);
    }
    None
}

/// Asserts that a copy holds the frontend lock until it is done: a take of the same lock from
/// inside the copy is refused, and the lock is free again when the copy has ended.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn the_lock_is_held_while_the_copy_runs_and_free_afterwards() {
    let dir = scratch("copy-lock");
    let written = built(&dir);
    let name = lock::canonical(&written.db).expect("the file is there");
    WATCHED.set(name.clone()).expect("set once");
    let target = Database::with_schema();
    let arguments = args(&written.db, &target.url(), true);
    let seams = with_tamper(try_the_lock);
    let (result, _report) = run_with(&arguments, &seams);
    result.expect("the copy is accepted");
    let seen = SEEN.load(Ordering::SeqCst);
    assert_eq!(seen, 2, "the lock was refused during the copy");
    drop(lock::take(&name).expect("the lock is free once the copy has ended"));
}

fn tool(argv: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kbf-state-copy"))
        .args(argv)
        .output()
        .expect("run kbf-state-copy")
}

/// Asserts that the binary dry-runs with status 0 and writes nothing, copies with status 0,
/// and then refuses a second copy with status 2 and the reason on the standard error.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the CI Postgres jobs run it"]
fn the_binary_dry_runs_then_copies_then_refuses_a_second_copy() {
    let dir = scratch("copy-binary");
    let db = golden_copy(&dir);
    let target = Database::with_schema();
    let path = db.to_str().expect("a UTF-8 path");
    let url = target.url();
    let dry = tool(&[path, url.as_str()]);
    assert_eq!(dry.status.code(), Some(0));
    assert_eq!(shape(&mut target.client()), [0, 0, 0, 0]);
    let said = String::from_utf8_lossy(&dry.stdout);
    starts(&said, "kbf-state-copy (dry run)\n");
    let copy = tool(&[path, url.as_str(), "--apply"]);
    assert_eq!(copy.status.code(), Some(0));
    let said = String::from_utf8_lossy(&copy.stdout);
    has(&said, "10 families match");
    assert_eq!(count(&mut target.client(), "kv"), 14);
    let again = tool(&[path, url.as_str(), "--apply"]);
    assert_eq!(again.status.code(), Some(2));
    let said = String::from_utf8_lossy(&again.stderr);
    starts(&said, "kbf-state-copy: refused: ");
}
