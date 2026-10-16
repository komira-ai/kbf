//! The state store trait.

use crate::clock::StoreTime;
use crate::types::{Changes, Commit, CommitStats, Conflict, Revision, StoreError, Txn, Versioned};

/// A durable, ordered key-value store with transactions, counters, time-to-live keys and
/// a change feed. The contract is clauses C1 to C7 of the architecture design; the
/// `conformance` module is the executable form every backend must pass.
///
/// Calls block until done; the SQLite backend's writer thread and the Postgres backend's
/// connections are behind them. A caller on an async runtime calls from a blocking task.
pub trait Store: Send + Sync {
    /// The group-commit entry point. Applies each transaction in order, each atomically
    /// and on its own precondition, and returns one result per transaction in the same
    /// order. The batch is made durable with one sync, and every result is returned only
    /// after that sync (C6, C7). A backend with a writer thread feeds the transactions
    /// of concurrent [`Store::txn`] calls through the same path.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails; no result in the batch is then durable.
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError>;

    /// The live value of `key`. An expired key is absent whether or not it was reaped.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError>;

    /// Every live key that starts with `prefix`, in key order.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError>;

    /// At most `limit` feed rows with a revision after `after`, in revision order, never
    /// past a revision that can still commit (C5).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError>;

    /// Removes every key the store's clock has expired, writing an `Expire` row for each,
    /// and returns how many. The reaper task calls this; reads do not wait for it (C4).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn reap_expired(&self) -> Result<usize, StoreError>;

    /// The commit counters.
    fn commit_stats(&self) -> CommitStats;

    /// The store's clock reading, the one expiry is judged by.
    fn now(&self) -> StoreTime;

    /// Applies one transaction: a batch of one.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn txn(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        // A backend returns one result per transaction; an empty answer is its bug.
        Ok(self.apply_batch(vec![txn])?.swap_remove(0))
    }
}
