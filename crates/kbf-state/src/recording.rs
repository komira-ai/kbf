//! A [`Store`] that forwards to another and remembers every write it was asked for, so
//! a test can assert which keys a component touched (and with what kind of operation)
//! after the fact. Test support: it lives here and not in `kbf-testkit` because that
//! crate depends on no kbf crate, and a store wrapper needs this one.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::clock::StoreTime;
use crate::store::{LivenessGuard, Store};
use crate::types::{
    Changes, Commit, CommitStats, Cond, Conflict, Op, Revision, ScanPage, ScanRange, StoreError,
    Txn, Versioned,
};

/// What a recorded write did to its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    /// A put, with or without a time to live.
    Put,
    /// A delete.
    Delete,
    /// A counter increment; the recorded key is the counter's name.
    Next,
}

/// One write a [`RecordingStore`] was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    /// The key (or the counter's name for [`WriteKind::Next`]).
    pub key: String,
    /// The kind of operation.
    pub kind: WriteKind,
    /// Whether its transaction committed. A write refused (a failed precondition, a
    /// fence, a store failure) is recorded too, with `false`: the question a test asks
    /// is usually which keys a component *tried* to write.
    pub committed: bool,
}

#[derive(Default)]
struct Log {
    writes: Vec<Write>,
    reaps: usize,
}

/// Forwards every call to the store underneath and records the writes. A
/// precondition-only operation (a check) is not a write and is not recorded, so a
/// fence's own check of the generation row does not show up.
pub struct RecordingStore {
    inner: Arc<dyn Store>,
    log: Mutex<Log>,
}

impl RecordingStore {
    /// Records the writes made through the returned store to `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Store>) -> RecordingStore {
        RecordingStore {
            inner,
            log: Mutex::new(Log::default()),
        }
    }

    fn log(&self) -> MutexGuard<'_, Log> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every recorded write, in the order it was asked for (the operations of one
    /// transaction in order, a batch's transactions in order).
    #[must_use]
    pub fn writes(&self) -> Vec<Write> {
        self.log().writes.clone()
    }

    /// The distinct keys written, sorted.
    #[must_use]
    pub fn keys(&self) -> BTreeSet<String> {
        self.log().writes.iter().map(|w| w.key.clone()).collect()
    }

    /// How many reaps were asked for (a reap writes `Expire` rows for keys the caller
    /// does not name, so it is counted, not listed).
    #[must_use]
    pub fn reaps(&self) -> usize {
        self.log().reaps
    }

    /// Forgets what was recorded so far.
    pub fn clear(&self) {
        *self.log() = Log::default();
    }

    fn record(&self, txn: &Txn, committed: bool) {
        let mut log = self.log();
        for op in txn.ops() {
            let (key, kind) = match op {
                Op::Put { key, .. } => (key, WriteKind::Put),
                Op::Delete { key, .. } => (key, WriteKind::Delete),
                Op::Next { counter } => (counter, WriteKind::Next),
                Op::Check { .. } => continue,
            };
            log.writes.push(Write {
                key: key.clone(),
                kind,
                committed,
            });
        }
    }
}

impl Store for RecordingStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let result = self.inner.apply_batch(batch.clone());
        for (index, txn) in batch.iter().enumerate() {
            let committed =
                matches!(&result, Ok(results) if matches!(results.get(index), Some(Ok(_))));
            self.record(txn, committed);
        }
        result
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.log().reaps += 1;
        self.inner.reap_expired()
    }

    fn reap_expired_if(
        &self,
        key: &str,
        cond: Cond,
    ) -> Result<Result<usize, Conflict>, StoreError> {
        self.log().reaps += 1;
        self.inner.reap_expired_if(key, cond)
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }

    fn demote(&self) -> Result<(), StoreError> {
        self.inner.demote()
    }

    fn hold_liveness(&self, generation: u64) -> Result<LivenessGuard, StoreError> {
        self.inner.hold_liveness(generation)
    }

    fn holder_alive(&self, generation: u64) -> Result<bool, StoreError> {
        self.inner.holder_alive(generation)
    }
}
