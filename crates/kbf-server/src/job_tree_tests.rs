//! The `job-tree/` row: its key, its bytes, create-only, and what a failing store makes of it.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};

use super::{Recorded, key, record, record_blocking};

const ROOT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const COMMIT: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn fake() -> Fake {
    Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None)
}

fn at_1700000000123() -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_700_000_000_123)
}

fn value(store: &Fake) -> Vec<u8> {
    store.get(&key(ROOT)).expect("a read").expect("a row").value
}

/// Asserts the row's exact key and bytes: `job-tree/<root>` holding the release's commit and the
/// milliseconds since the Unix epoch at which it was first published, in that member order, with
/// no space. Catches: a key or a value another reader of the family would not parse, a member
/// out of order, a time in seconds or a commit shortened.
#[test]
fn the_row_is_the_release_and_the_first_publish_time() {
    let store = fake();

    let recorded = record(&store, ROOT, COMMIT, at_1700000000123).expect("recorded");

    assert_eq!(recorded, Recorded::First);
    assert_eq!(
        key(ROOT),
        "job-tree/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        value(&store),
        br#"{"release":"89abcdef0123456789abcdef0123456789abcdef","first_published_ms":1700000000123}"#
    );
}

/// Asserts that a row which exists is left as it is: another release publishing the same tree
/// later is told so and changes neither the release nor the time. Catches: a rollback that
/// makes the first publish last.
#[test]
fn an_existing_row_is_left_as_it_is() {
    let store = fake();
    record(&store, ROOT, COMMIT, at_1700000000123).expect("first");
    let before = store.get(&key(ROOT)).expect("read").expect("row");

    let later = record(&store, ROOT, "another-release", || {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    })
    .expect("again");

    assert_eq!(later, Recorded::Again);
    assert_eq!(store.get(&key(ROOT)).expect("read"), Some(before));
}

/// Asserts that two trees have two rows, and that a release whose name needs JSON escaping is
/// written as valid JSON. Catches: a key shared by trees, and a value built by pasting text.
#[test]
fn each_tree_has_a_row_and_the_release_is_a_json_string() {
    let store = fake();
    let other = "f".repeat(64);

    record(&store, ROOT, "a \"quoted\" release", at_1700000000123).expect("one");
    record(&store, &other, COMMIT, at_1700000000123).expect("two");

    assert_eq!(store.scan_prefix("job-tree/").expect("a scan").len(), 2);
    let parsed: serde_json::Value = serde_json::from_slice(&value(&store)).expect("JSON");
    assert_eq!(parsed["release"], "a \"quoted\" release");
    assert_eq!(parsed["first_published_ms"], 1_700_000_000_123_u64);
}

/// Asserts that a wall clock before the Unix epoch records time 0 instead of failing the start.
/// Catches: a start that cannot run on a host whose clock is not set.
#[test]
fn a_clock_before_the_epoch_records_zero() {
    let store = fake();

    record(&store, ROOT, COMMIT, || UNIX_EPOCH - Duration::from_secs(5)).expect("recorded");

    assert_eq!(
        value(&store),
        br#"{"release":"89abcdef0123456789abcdef0123456789abcdef","first_published_ms":0}"#
    );
}

/// A store that fails every write and answers every read as an empty store.
pub(crate) struct Refusing(pub(crate) Fake);

impl Store for Refusing {
    fn apply_batch(&self, _: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Err(StoreError::Failed("the disk is full".to_owned()))
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.0.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.0.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.0.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.0.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.0.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.0.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.0.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.0.now()
    }
}

/// Asserts that a store that fails the write is an error naming the row's key and the cause,
/// and not taken for an existing row. Catches: a failed record swallowed, so the start goes on
/// with no record.
#[test]
fn a_failing_store_is_an_error_naming_the_row() {
    let error = record(&Refusing(fake()), ROOT, COMMIT, at_1700000000123).expect_err("refused");

    assert_eq!(
        error.to_string(),
        format!("{}: state store failure: the disk is full", key(ROOT))
    );
}

/// A store whose write panics.
struct Panicking(Fake);

impl Store for Panicking {
    fn apply_batch(&self, _: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        panic!("the store's writer is gone")
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.0.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.0.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.0.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.0.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.0.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.0.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.0.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.0.now()
    }
}

/// Asserts that the blocking form writes the row as [`record`] does, and that a store that panics
/// is an error naming the row and not a panic of the start. Catches: a join error unwrapped, which
/// would take the frontend's start down with the store's thread.
#[tokio::test]
async fn the_blocking_form_writes_the_row_and_a_panicking_store_is_an_error() {
    let store = Arc::new(fake());
    let first = record_blocking(
        store.clone(),
        ROOT.to_owned(),
        COMMIT.to_owned(),
        at_1700000000123,
    );

    assert_eq!(first.await.expect("recorded"), Recorded::First);
    assert_eq!(
        value(&store),
        br#"{"release":"89abcdef0123456789abcdef0123456789abcdef","first_published_ms":1700000000123}"#
    );

    let panicking = Arc::new(Panicking(fake()));
    let error = record_blocking(
        panicking,
        ROOT.to_owned(),
        COMMIT.to_owned(),
        at_1700000000123,
    )
    .await
    .expect_err("refused");
    let said = error.to_string();
    assert!(
        said.starts_with(&format!("{}: state store failure: ", key(ROOT))),
        "{said}"
    );
    assert!(said.contains("the store's writer is gone"), "{said}");
}
