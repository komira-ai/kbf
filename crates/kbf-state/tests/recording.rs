//! `RecordingStore`: forwards everything, and remembers each write it was asked for.
//!
//! Every test names the defect it catches.

use std::sync::Arc;
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::recording::{RecordingStore, Write, WriteKind};
use kbf_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

/// A store that fails every call with `StoreError::Fenced`.
struct Refusing;

impl Store for Refusing {
    fn apply_batch(&self, _batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Err(StoreError::Fenced)
    }

    fn get(&self, _key: &str) -> Result<Option<Versioned>, StoreError> {
        Err(StoreError::Fenced)
    }

    fn scan_prefix(&self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        Err(StoreError::Fenced)
    }

    fn scan_range(&self, _scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        Err(StoreError::Fenced)
    }

    fn changes(&self, _after: Revision, _limit: usize) -> Result<Changes, StoreError> {
        Err(StoreError::Fenced)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Err(StoreError::Fenced)
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats::default()
    }

    fn now(&self) -> StoreTime {
        StoreTime::ZERO
    }
}

fn write(key: &str, kind: WriteKind, committed: bool) -> Write {
    Write {
        key: key.to_owned(),
        kind,
        committed,
    }
}

// Catches a recorder that misses a kind of write or misreports it: puts (with and without
// a time to live), deletes and counters are recorded in order with their kind; a check
// is a precondition, not a write, and is not.
#[test]
fn every_kind_of_write_is_recorded_in_order_and_a_check_is_not() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(
            Txn::new()
                .put("a", "1", Cond::Any)
                .put_ttl("b", "2", Duration::from_secs(5), Cond::Any)
                .check("watched", Cond::Any)
                .delete("a", Cond::Any)
                .next("n"),
        )
        .expect("store")
        .expect("commit");
    assert_eq!(
        recording.writes(),
        [
            write("a", WriteKind::Put, true),
            write("b", WriteKind::Put, true),
            write("a", WriteKind::Delete, true),
            write("n", WriteKind::Next, true),
        ]
    );
    assert_eq!(
        recording.keys().into_iter().collect::<Vec<_>>(),
        ["a", "b", "n"]
    );
}

// Catches a recorder that forgets what was refused: a write whose precondition fails is
// recorded as not committed (the keys a component tried to write are what a test needs),
// and in a batch each transaction has its own outcome.
#[test]
fn a_refused_write_is_recorded_as_not_committed() {
    let recording = RecordingStore::new(fake());
    let results = recording
        .apply_batch(vec![
            Txn::new().put("k", "v", Cond::Absent),
            Txn::new().put("k", "w", Cond::Absent),
            Txn::new().put("z", "x", Cond::Any),
        ])
        .expect("store");
    assert!(results[0].is_ok() && results[1].is_err() && results[2].is_ok());
    assert_eq!(
        recording.writes(),
        [
            write("k", WriteKind::Put, true),
            write("k", WriteKind::Put, false),
            write("z", WriteKind::Put, true),
        ]
    );
}

/// Asserts that a store that fails the whole call still has the write recorded, as not
/// committed, and that the error reaches the caller unchanged.
#[test]
fn a_write_the_store_fails_is_recorded_and_the_error_passes_through() {
    let recording = RecordingStore::new(Arc::new(Refusing));
    let err = recording
        .txn(Txn::new().put("mine", "v", Cond::Any))
        .unwrap_err();
    assert_eq!(err, StoreError::Fenced);
    assert_eq!(recording.writes(), [write("mine", WriteKind::Put, false)]);
}

/// Asserts that a reap is counted, and that `clear` resets the writes and the reaps to none.
#[test]
fn reaps_are_counted_and_clear_forgets_everything() {
    let recording = RecordingStore::new(fake());
    recording
        .txn(Txn::new().put("k", "v", Cond::Any))
        .expect("store")
        .expect("commit");
    assert_eq!(recording.reap_expired(), Ok(0));
    assert_eq!(recording.reaps(), 1);
    recording.clear();
    assert_eq!((recording.reaps(), recording.writes()), (0, Vec::new()));
}

/// Asserts that reads, the feed, the clock and the counters answer as the store underneath
/// does, and that none of them is recorded.
#[test]
fn everything_else_is_forwarded_and_not_recorded() {
    let inner = fake();
    let recording = RecordingStore::new(Arc::clone(&inner));
    recording
        .txn(
            Txn::new()
                .put("p/a", "1", Cond::Any)
                .put("p/b", "2", Cond::Any),
        )
        .expect("store")
        .expect("commit");
    recording.clear();
    assert_eq!(
        recording.get("p/a").expect("get"),
        inner.get("p/a").expect("get")
    );
    assert_eq!(recording.scan_prefix("p/").expect("scan").len(), 2);
    let page = recording
        .scan_range(ScanRange::new("p/", std::num::NonZeroUsize::MIN))
        .expect("range");
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        recording
            .changes(Revision::ZERO, 10)
            .expect("feed")
            .rows
            .len(),
        2
    );
    assert_eq!(recording.commit_stats(), inner.commit_stats());
    assert_eq!(recording.now(), inner.now());
    assert_eq!(recording.writes(), Vec::new());
}
