//! The state store trait.

use std::num::NonZeroUsize;
use std::ops::Bound;

use crate::clock::StoreTime;
use crate::types::{
    Changes, Commit, CommitStats, Conflict, ResumeToken, Revision, ScanPage, StoreError, Txn,
    Versioned,
};

/// A durable, ordered key-value store with transactions, counters, time-to-live keys and
/// a change feed. The contract is clauses C1 to C8 of the architecture design; the
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

    /// A page of the live keys that start with `prefix` and lie within `start`, in the key
    /// order of [`Store::scan_prefix`] (bytewise), at most `limit` of them (C8). A key
    /// before the prefix as `start` begins at the prefix; one past it gives an empty page.
    /// [`ScanPage::resume`] is set when a live key under the prefix follows the last row,
    /// and its [`crate::ResumeToken::start`] begins the next page exactly after that row.
    ///
    /// Under concurrent commits each page is read from one snapshot of committed state:
    /// it holds every transaction acknowledged before the call and none still in flight,
    /// each one whole or not at all. Pages are not one snapshot. A key committed behind
    /// the token after its page is not returned; one ahead of it is; a key deleted or
    /// expired before its page is read is absent; a key updated between pages shows the
    /// value of the page that returns it. So every key live under the prefix for the whole
    /// scan is returned exactly once, and the keys of a scan strictly ascend.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn scan_range(
        &self,
        prefix: &str,
        start: Bound<&str>,
        limit: NonZeroUsize,
    ) -> Result<ScanPage, StoreError>;

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

/// Where a C8 scan begins: at a key or just after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lower<'a> {
    /// At this key and after.
    From(&'a str),
    /// After this key.
    After(&'a str),
}

impl<'a> Lower<'a> {
    /// Where a scan of `prefix` from `start` begins: `start` when it is at or past the
    /// prefix, else the prefix itself (every key under it is at or after it).
    pub(crate) fn of(prefix: &'a str, start: Bound<&'a str>) -> Lower<'a> {
        match start {
            Bound::Included(key) if key >= prefix => Lower::From(key),
            Bound::Excluded(key) if key >= prefix => Lower::After(key),
            _ => Lower::From(prefix),
        }
    }
}

/// A C8 page from up to `limit + 1` live rows read in key order: the first `limit`, and
/// a resume token when there was one more.
pub(crate) fn page_of(mut rows: Vec<(String, Versioned)>, limit: NonZeroUsize) -> ScanPage {
    let more = rows.len() > limit.get();
    rows.truncate(limit.get());
    let resume = match rows.last() {
        Some((key, _)) if more => Some(ResumeToken::after(key.clone())),
        _ => None,
    };
    ScanPage { rows, resume }
}
