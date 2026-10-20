//! A state store that misbehaves on purpose, for the tests that need the moment between a
//! reader's look and its write, or a store that answers wrongly.
//!
//! [`Meddle::before_each`] calls a hook with every transaction just before the batch that
//! holds it is applied: the hook writes to the real store (not through the meddler), so the
//! transaction that follows meets a row that moved, as it would under a second controller.
//! [`Meddle::without_counters`] applies every transaction and answers it with no value for
//! the counters it advanced. [`Meddle::failing_writes_to`] fails every batch that writes a
//! key, the way a disk that is gone does, and applies nothing of it.

#![allow(dead_code)]

use std::sync::{Arc, Mutex, PoisonError};

use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Op, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};

type Hook = Box<dyn FnMut(&Txn) + Send>;

/// A store that wraps another.
pub struct Meddle {
    inner: Arc<dyn Store>,
    before: Mutex<Option<Hook>>,
    strip_counters: bool,
    failing: Option<&'static str>,
}

impl Meddle {
    /// `inner`, with `hook` called for every transaction before it is applied.
    pub fn before_each(
        inner: Arc<dyn Store>,
        hook: impl FnMut(&Txn) + Send + 'static,
    ) -> Arc<dyn Store> {
        Arc::new(Self {
            inner,
            before: Mutex::new(Some(Box::new(hook))),
            strip_counters: false,
            failing: None,
        })
    }

    /// `inner`, answering every transaction with the counters it advanced left out.
    pub fn without_counters(inner: Arc<dyn Store>) -> Arc<dyn Store> {
        Arc::new(Self {
            inner,
            before: Mutex::new(None),
            strip_counters: true,
            failing: None,
        })
    }

    /// `inner`, failing every batch with a transaction that writes `key`.
    pub fn failing_writes_to(inner: Arc<dyn Store>, key: &'static str) -> Arc<dyn Store> {
        Arc::new(Self {
            inner,
            before: Mutex::new(None),
            strip_counters: false,
            failing: Some(key),
        })
    }
}

/// Whether `txn` writes (puts or deletes) the key `key`.
pub fn writes(txn: &Txn, key: &str) -> bool {
    txn.ops().iter().any(|op| {
        matches!(op, Op::Put { .. } | Op::Delete { .. })
            && op.key_cond().is_some_and(|(written, _)| written == key)
    })
}

impl Store for Meddle {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if let Some(key) = self.failing
            && batch.iter().any(|txn| writes(txn, key))
        {
            return Err(StoreError::Failed(format!(
                "the disk holding {key} is gone"
            )));
        }
        if let Some(hook) = self
            .before
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            for txn in &batch {
                hook(txn);
            }
        }
        let mut done = self.inner.apply_batch(batch)?;
        if self.strip_counters {
            for commit in done.iter_mut().flatten() {
                commit.counters.clear();
            }
        }
        Ok(done)
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
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}
