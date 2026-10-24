//! The shared-writers marker: [`share`] and the generation row it leaves, and the lock that
//! keeps a SQLite state file to one process.

mod common;

use std::sync::Arc;

use common::scratch;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::sqlite::{Options, SqliteError, SqliteStore};
use kbf_state::{
    Clock, Cond, GENERATION, ManualClock, SHARED_WRITERS, Store, StoreError, Txn, share,
};

fn fake() -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn open(dir: &std::path::Path) -> Result<SqliteStore, SqliteError> {
    let clock: Arc<dyn Clock> = Arc::new(kbf_state::SystemClock::default());
    SqliteStore::open(&dir.join("state.db"), clock, Options::default())
}

/// Asserts that a second open of a state file that a store holds open is refused with
/// `Locked`, and that the file opens again once the first store is dropped.
#[test]
fn a_second_open_of_a_held_state_file_is_locked_until_the_first_store_is_dropped() {
    let dir = scratch("locked");
    let first = open(&dir).expect("open");
    assert!(
        matches!(open(&dir), Err(SqliteError::Locked { .. })),
        "the lock is held while the first store is open"
    );
    drop(first);
    drop(open(&dir).expect("the file is free once the first store is dropped"));
}

/// What a [`Meddled`] store fails.
#[derive(Clone, Copy)]
enum Fail {
    Nothing,
    Reads,
    Writes,
}

/// A store that runs `after_get` after every read of the generation row, where another
/// process can write between the read and the write that follows it, and that fails the
/// reads or the writes it is told to.
struct Meddled {
    inner: Arc<dyn Store>,
    after_get: Box<dyn Fn() + Send + Sync>,
    fail: Fail,
}

impl Meddled {
    fn new(
        inner: &Arc<dyn Store>,
        fail: Fail,
        after_get: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::clone(inner),
            after_get: Box::new(after_get),
            fail,
        }
    }
}

impl Store for Meddled {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, StoreError> {
        if matches!(self.fail, Fail::Writes) {
            return Err(StoreError::Failed("writes fail".to_owned()));
        }
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, StoreError> {
        if matches!(self.fail, Fail::Reads) {
            return Err(StoreError::Failed("reads fail".to_owned()));
        }
        let row = self.inner.get(key)?;
        if key == GENERATION {
            (self.after_get)();
        }
        Ok(row)
    }
    fn scan_prefix(&self, p: &str) -> Result<Vec<(String, kbf_state::Versioned)>, StoreError> {
        self.inner.scan_prefix(p)
    }
    fn scan_range(&self, s: kbf_state::ScanRange<'_>) -> Result<kbf_state::ScanPage, StoreError> {
        self.inner.scan_range(s)
    }
    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> kbf_state::CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> kbf_state::StoreTime {
        self.inner.now()
    }
}

/// Asserts that the marker is the bytes older builds are refused by, and is not eight
/// bytes: a build that reads the generation row as eight big-endian bytes finds it corrupt.
#[test]
fn the_shared_marker_is_not_eight_bytes() {
    assert_eq!(SHARED_WRITERS, b"shared-writers".as_slice());
    assert_ne!(SHARED_WRITERS.len(), 8);
}

/// Asserts that sharing a store with no generation row writes the marker, and that sharing a
/// store that holds the marker writes nothing (the row keeps its version).
#[test]
fn sharing_an_empty_store_writes_the_marker_and_sharing_it_again_writes_nothing() {
    let store = fake();
    share(store.as_ref()).expect("share");
    let row = store.get(GENERATION).expect("get").expect("a row");
    assert_eq!(row.value, SHARED_WRITERS);
    share(store.as_ref()).expect("share again");
    assert_eq!(store.get(GENERATION).expect("get"), Some(row), "it wrote");
}

/// Asserts that sharing a store whose generation row holds neither the marker nor eight
/// bytes is refused with an error that says so, and that the row is left as it was.
#[test]
fn sharing_a_store_with_a_junk_generation_row_is_refused_and_leaves_the_row() {
    let store = fake();
    store
        .txn(Txn::new().put(GENERATION, "junk", Cond::Any))
        .expect("s")
        .expect("c");
    let before = store.get(GENERATION).expect("get");

    let refused = share(store.as_ref()).expect_err("refused");

    assert!(
        matches!(&refused, StoreError::Failed(why) if why.contains("neither a generation")),
        "{refused:?}"
    );
    assert_eq!(store.get(GENERATION).expect("get"), before, "it wrote");
}

/// Asserts that sharing a store whose generation row holds a generation (eight bytes)
/// replaces the generation with the marker, in a write that moves the row's version.
#[test]
fn sharing_a_store_that_holds_a_generation_replaces_it_with_the_marker() {
    let store = fake();
    store
        .txn(Txn::new().put(GENERATION, 7_u64.to_be_bytes(), Cond::Any))
        .expect("s")
        .expect("c");
    let before = store.get(GENERATION).expect("get").expect("a row");
    share(store.as_ref()).expect("share");
    let row = store.get(GENERATION).expect("get").expect("a row");
    assert_eq!(row.value, SHARED_WRITERS);
    assert_ne!(
        row.version, before.version,
        "the generation was not replaced"
    );
}

/// Asserts that a row another process writes between the read and the write is read again:
/// a generation written in that gap is replaced by the marker, and the call succeeds.
#[test]
fn a_row_written_between_the_read_and_the_write_is_read_again() {
    let raw = fake();
    let rival = Arc::clone(&raw);
    let once = std::sync::Once::new();
    let racing = Meddled::new(&raw, Fail::Nothing, move || {
        once.call_once(|| {
            let put = Txn::new().put(GENERATION, 1_u64.to_be_bytes(), Cond::Any);
            rival.txn(put).expect("s").expect("c");
        });
    });
    share(&racing).expect("share");
    let row = raw.get(GENERATION).expect("get").expect("a row");
    assert_eq!(row.value, SHARED_WRITERS);
}

/// Asserts that a row that changes under every try is given up on with an error that says
/// so, and that the call does not leave the marker over a row it never read.
#[test]
fn a_row_that_changes_under_every_try_is_given_up_on() {
    let raw = fake();
    let rival = Arc::clone(&raw);
    let moved = std::sync::atomic::AtomicU64::new(0);
    let racing = Meddled::new(&raw, Fail::Nothing, move || {
        let n = moved.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let put = Txn::new().put(GENERATION, n.to_be_bytes(), Cond::Any);
        rival.txn(put).expect("s").expect("c");
    });
    let refused = share(&racing).expect_err("given up");
    assert!(
        matches!(&refused, StoreError::Failed(why) if why.contains("kept changing")),
        "{refused:?}"
    );
    let row = raw.get(GENERATION).expect("get").expect("a row");
    assert_ne!(row.value, SHARED_WRITERS);
}

/// Asserts that a store that fails a read or a write is reported as failed, and that nothing
/// is marked.
#[test]
fn a_store_that_fails_is_reported_and_left_unmarked() {
    for fail in [Fail::Reads, Fail::Writes] {
        let raw = fake();
        let broken = Meddled::new(&raw, fail, || {});
        assert!(matches!(share(&broken), Err(StoreError::Failed(_))));
        assert_eq!(raw.get(GENERATION).expect("get"), None);
    }
}
