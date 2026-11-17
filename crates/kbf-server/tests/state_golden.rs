//! The state store's on-disk compatibility: a SQLite state store written by an earlier
//! release stays readable by this one, row for row.
//!
//! `tests/golden/state/` holds, per released schema version `N`, a real state store file
//! (`vN.db`, written by the code of the release) and the rows it held (`vN.rows`, a text
//! rendering). This test opens a copy of every such file with the code under test and
//! requires:
//!
//! - the file opens (so a bumped `SCHEMA_VERSION` with no way to read the old file is red,
//!   and so is a renamed or retyped column);
//! - every row reads back unchanged: keys, values, versions, expiry times, the change
//!   feed, the head revision and a counter;
//! - the executor's rows over the file take up what an earlier release wrote, each through
//!   the reader that uses it: a `done/` row with no `doneof/` row beside it (as an earlier
//!   release wrote it) is not an error, `meta/next_op` is continued (no id an earlier release gave is given
//!   again), and the rows of families this release no longer reads (`op/`, `queue/`,
//!   `lease/`, `loss/`, `fate/`, `cordon/`, `session/`) are left as they are and take up
//!   nothing;
//! - there is a golden for the current `SCHEMA_VERSION`, so a bump cannot ship without a
//!   new golden (and, until a migration exists, without the old ones being refused here).
//!
//! A golden is written once per release, by a person: `cargo test -p kbf-server --test
//! state_golden -- --ignored write_golden` writes `v<SCHEMA_VERSION>.db` and `.rows`, and
//! refuses to replace a file that exists. Commit the two files on the release commit.
//! The clock of the store is a manual one that never advances, so expiry times are
//! deterministic.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_front::Finished;
use kbf_proto::google::rpc;
use kbf_server::executor::rows::Rows;
use kbf_state::sqlite::{Options, SCHEMA_VERSION, SqliteStore};
use kbf_state::{ManualClock, Revision, Store, Txn};
use kbf_types::{ActionKey, Digest, DigestFunction, OperationId};
use tonic::Code;

/// The counter the writer advances twice, so a golden holds a counter row.
const COUNTER: &str = "golden/counter";
const COUNTER_AT_WRITE: u64 = 2;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/state")
}

fn open(path: &Path) -> SqliteStore {
    SqliteStore::open(path, Arc::new(ManualClock::default()), Options::default())
        .expect("a SQLite state store")
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        write!(out, "{byte:02x}").expect("a string takes text");
    }
    out
}

/// Every row the store reads back, as text: the live keys with version and expiry, the
/// change feed with its cursor.
fn render(store: &dyn Store) -> String {
    let mut out = String::new();
    for (key, row) in store.scan_prefix("").expect("a scan") {
        let expiry = row.expires_at.map_or_else(
            || "-".to_owned(),
            |t| t.as_duration().as_nanos().to_string(),
        );
        writeln!(
            out,
            "kv {key} version={} expires_ns={expiry} value={}",
            row.version.0,
            hex(&row.value)
        )
        .expect("a string takes text");
    }
    let feed = store.changes(Revision::ZERO, 100_000).expect("the feed");
    for change in &feed.rows {
        let event = match &change.event {
            kbf_state::Event::Put(value) => format!("put {}", hex(value)),
            kbf_state::Event::Delete => "delete".to_owned(),
            kbf_state::Event::Expire => "expire".to_owned(),
        };
        writeln!(out, "feed {} {} {event}", change.revision.0, change.key)
            .expect("a string takes text");
    }
    writeln!(out, "cursor {}", feed.cursor.0).expect("a string takes text");
    writeln!(out, "counter {COUNTER} {COUNTER_AT_WRITE}").expect("a string takes text");
    out
}

/// The action of the tag `tag`.
fn action(tag: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
    }
}

/// The executor's op row of the action `tag`.
fn op_row(tag: u8) -> kbf_server::executor::rows::OpRow {
    let key = action(tag);
    kbf_server::executor::rows::OpRow {
        owner: String::new(),
        instance: key.instance,
        hash: key.action.hash_hex(),
        size: key.action.size_bytes,
        attempt: 0,
        job: format!("k0000000{tag}-op-0"),
        key: None,
        whole_machine: false,
    }
}

/// Writes `vN.db` and `vN.rows` into `dir` by driving the executor's rows over a new store:
///
/// - operation 0 is unfinished: the store holds its `exec/op/` row and its process's
///   `exec/live/` row;
/// - operation 1 finishes, so the store holds its `done/` and `doneof/` rows.
async fn write_into(dir: &Path) {
    let db = dir.join(format!("v{SCHEMA_VERSION}.db"));
    let rows = dir.join(format!("v{SCHEMA_VERSION}.rows"));
    assert!(
        !db.exists() && !rows.exists(),
        "{} exists: a golden is written once per release, never replaced",
        db.display()
    );
    let scratch = tempdir();
    let store = Arc::new(open(&scratch.join("state.db")));
    let mine = Rows::new(store.clone(), "golden");
    mine.beat(std::time::Duration::from_secs(60))
        .expect("a liveness row");
    let (unfinished, finished) = (
        mine.next_id().expect("an id"),
        mine.next_id().expect("an id"),
    );
    mine.put_op(unfinished, &op_row(1)).expect("an op row");
    mine.put_op(finished, &op_row(2)).expect("an op row");
    let answer = Finished::Failed(rpc::Status {
        code: Code::InvalidArgument as i32,
        message: "golden".to_owned(),
        details: Vec::new(),
    });
    mine.finish(
        finished,
        &action(2),
        &answer,
        std::time::Duration::from_secs(600),
    )
    .expect("an answer");
    drop(mine);

    for _ in 0..COUNTER_AT_WRITE {
        store
            .apply_batch(vec![Txn::new().next(COUNTER)])
            .expect("a counter");
    }
    let text = render(&*store);
    for prefix in ["exec/live/", "exec/op/", "done/", "doneof/", "meta/next_op"] {
        assert!(
            text.lines().any(|l| l.starts_with(&format!("kv {prefix}"))),
            "the golden holds no {prefix} row"
        );
    }
    std::fs::write(&rows, text).expect("the rows are written");
    drop(store);
    std::fs::copy(scratch.join("state.db"), &db).expect("the file is copied");
}

/// A scratch directory, removed when it drops.
fn tempdir() -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-state-golden")
}

/// Writes the golden of this release. Run by a person on the release commit (see the
/// module docs); it refuses to replace a file.
#[tokio::test(start_paused = true)]
#[ignore = "writes the golden of the current schema version; a release-time step"]
async fn write_golden() {
    write_into(&golden_dir()).await;
}

fn goldens() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(golden_dir())
        .expect("the golden directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "db"))
        .collect();
    found.sort();
    found
}

/// Catches: a change to the on-disk layout or to the rows' keys that an earlier release's
/// file does not survive: a schema version bumped with no way to read the old file, a
/// renamed or retyped column, a `done/` key format that no longer finds the rows already
/// written, a `lease/` key or value the farm no longer takes up. Each of those is a
/// rolling upgrade that loses or refuses the store.
#[tokio::test(start_paused = true)]
async fn every_golden_state_store_reads_back_unchanged() {
    let current = golden_dir().join(format!("v{SCHEMA_VERSION}.db"));
    assert!(
        current.exists(),
        "no golden for schema version {SCHEMA_VERSION}: write {} on the release commit",
        current.display()
    );
    let found = goldens();
    assert!(
        !found.is_empty(),
        "no golden at all: the test would pass empty"
    );
    for path in found {
        read_back(&path).await;
    }
}

/// The keys under each of the families this release no longer reads.
const RETIRED: [&str; 7] = [
    "op/", "queue/", "lease/", "loss/", "fate/", "cordon/", "session/",
];

fn keys_of(store: &SqliteStore, prefix: &str) -> Vec<String> {
    store
        .scan_prefix(prefix)
        .expect("a scan")
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

async fn read_back(golden: &Path) {
    let name = golden
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    let expected = std::fs::read_to_string(golden.with_extension("rows"))
        .unwrap_or_else(|e| panic!("{name} has no .rows beside it: {e}"));
    let scratch = tempdir();
    let copy = scratch.join("state.db");
    std::fs::copy(golden, &copy).expect("a copy");

    let store = Arc::new(open(&copy));
    assert_eq!(
        render(&*store),
        expected,
        "{name}: the rows read back changed"
    );
    // The counter continues from the stored value.
    let next = store
        .apply_batch(vec![Txn::new().next(COUNTER)])
        .expect("a counter")
        .remove(0)
        .expect("committed")
        .counters;
    assert_eq!(next, vec![COUNTER_AT_WRITE + 1], "{name}: the counter row");

    let [done_key] = &keys_of(&store, "done/")[..] else {
        panic!("{name}: expected one done/ row");
    };
    let done = OperationId(
        u64::from_str_radix(done_key.strip_prefix("done/").expect("a prefix"), 16).expect("hex"),
    );
    let stored_next = store
        .get("meta/next_op")
        .expect("a read")
        .expect("a next_op row")
        .value;
    let stored_next = u64::from_be_bytes(stored_next.as_slice().try_into().expect("eight bytes"));
    let before: Vec<Vec<String>> = RETIRED.iter().map(|p| keys_of(&store, p)).collect();

    let rows = Rows::new(store.clone(), "today");
    // An earlier release wrote a `done/` row with no `doneof/` row beside it; the executor names
    // an answer's action from `doneof/`, so it does not serve that answer by name (the row is
    // not corrupt: reading it is not an error).
    assert_eq!(
        rows.done(done).expect("the done/ row is not an error"),
        None,
        "{name}: a done/ row with no doneof/ row"
    );
    let first = rows.next_id().expect("an id");
    assert!(
        first.0 >= stored_next && first > done,
        "{name}: id {first:?} may be one an earlier release gave (next_op was {stored_next})"
    );
    assert_eq!(
        rows.orphans().expect("a scan"),
        [],
        "{name}: the rows of a release before the executor were taken for operations to run"
    );
    assert_eq!(rows.unknown_rows(), 0, "{name}");
    let after: Vec<Vec<String>> = RETIRED.iter().map(|p| keys_of(&store, p)).collect();
    assert_eq!(after, before, "{name}: rows of retired families changed");
}

/// Catches: a writer that replaces a released golden (the file the next release must still
/// read would then be one the new code wrote).
#[tokio::test(start_paused = true)]
async fn the_writer_refuses_to_replace_a_golden() {
    let scratch = tempdir();
    let dir = scratch.to_path_buf();
    std::fs::write(dir.join(format!("v{SCHEMA_VERSION}.db")), b"kept").expect("a file");
    let wrote = tokio::spawn(async move { write_into(&dir).await }).await;
    assert!(wrote.is_err(), "the writer replaced an existing golden");
}

/// Catches: a golden writer whose files the reader refuses or reads back changed: what the
/// writer puts in an empty directory (the executor's operation, liveness and answer rows, the
/// counter) reads back row for row from the copied file, and the answer it wrote is the one the
/// executor's rows return by name with its action.
#[tokio::test(start_paused = true)]
async fn what_the_writer_writes_the_reader_reads_back() {
    let scratch = tempdir();
    let dir = scratch.to_path_buf();
    write_into(&dir).await;
    let db = dir.join(format!("v{SCHEMA_VERSION}.db"));
    let rows = std::fs::read_to_string(db.with_extension("rows")).expect("the rows file");

    let store = Arc::new(open(&db));
    assert_eq!(render(&*store), rows, "the file reads back changed");
    let [done_key] = &keys_of(&store, "done/")[..] else {
        panic!("one operation finished");
    };
    let finished = OperationId(
        u64::from_str_radix(done_key.strip_prefix("done/").expect("a prefix"), 16).expect("hex"),
    );
    let [op_key] = &keys_of(&store, "exec/op/")[..] else {
        panic!("one operation is unfinished");
    };
    let unfinished = OperationId(
        u64::from_str_radix(op_key.strip_prefix("exec/op/").expect("a prefix"), 16).expect("hex"),
    );
    let reader = Rows::new(store, "reader");
    let (key, answer) = reader
        .done(finished)
        .expect("the answer decodes")
        .expect("the finished operation has its answer");
    assert_eq!(key, action(2));
    assert!(
        matches!(&answer, Finished::Failed(s) if s.code == Code::InvalidArgument as i32),
        "{answer:?}"
    );
    assert_eq!(
        reader.op(unfinished).expect("a read"),
        Some(kbf_server::executor::rows::OpRow {
            owner: "golden".to_owned(),
            ..op_row(1)
        }),
        "the unfinished operation keeps its row"
    );
}
