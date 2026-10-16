//! Red-first stub of the SQLite backend: the tests land first and fail on `todo!()`; the
//! implementation follows in the next commit.

// The stubs use none of these yet; the build is `-D warnings`.
#[allow(dead_code)]
mod error;
#[allow(dead_code)]
mod guard;
#[allow(dead_code)]
mod schema;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub use error::SqliteError;

use crate::GROUP_COMMIT_WINDOW;
use crate::clock::{Clock, StoreTime};
use crate::store::Store;
use crate::types::{Changes, Commit, CommitStats, Conflict, Revision, StoreError, Txn, Versioned};

/// How the store groups writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// How long the writer waits, after the first request of a group, for more to join it.
    pub window: Duration,
    /// The group is committed as soon as it holds this many transactions, window or not.
    pub max_group: usize,
    /// A group whose commit latency passes this is counted in
    /// [`CommitMetrics::over_limit`].
    pub latency_limit: Duration,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            window: GROUP_COMMIT_WINDOW,
            max_group: 4096,
            latency_limit: Duration::from_millis(100),
        }
    }
}

/// Commit latency figures, for the operator's metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitMetrics {
    /// Groups committed.
    pub groups: u64,
    /// The most transactions any group held.
    pub largest_group: u64,
    /// The sum of the groups' latencies.
    pub total_latency: Duration,
    /// The longest any group took.
    pub max_latency: Duration,
    /// How many groups passed [`Options::latency_limit`].
    pub over_limit: u64,
}

/// The SQLite state store.
#[derive(Debug)]
pub struct SqliteStore {
    _stub: (),
}

impl SqliteStore {
    /// Opens the state file `path`.
    ///
    /// # Errors
    /// [`SqliteError`].
    pub fn open(
        _path: &Path,
        _clock: Arc<dyn Clock>,
        _options: Options,
    ) -> Result<SqliteStore, SqliteError> {
        todo!("av2-03: SqliteStore::open")
    }

    #[allow(dead_code)]
    fn open_checked(
        _path: &Path,
        _clock: Arc<dyn Clock>,
        _options: Options,
        _probe: &dyn Fn(&Path) -> io::Result<u32>,
    ) -> Result<SqliteStore, SqliteError> {
        todo!("av2-03: SqliteStore::open_checked")
    }

    /// The state file.
    #[must_use]
    pub fn path(&self) -> &Path {
        todo!("av2-03: SqliteStore::path")
    }

    /// The clock the store was opened with.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        todo!("av2-03: SqliteStore::clock")
    }

    /// The store's incarnation.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        todo!("av2-03: SqliteStore::epoch")
    }

    /// The commit latency figures so far.
    #[must_use]
    pub fn commit_metrics(&self) -> CommitMetrics {
        todo!("av2-03: SqliteStore::commit_metrics")
    }
}

impl Store for SqliteStore {
    fn apply_batch(&self, _batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        todo!("av2-03: SqliteStore::apply_batch")
    }

    fn get(&self, _key: &str) -> Result<Option<Versioned>, StoreError> {
        todo!("av2-03: SqliteStore::get")
    }

    fn scan_prefix(&self, _prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        todo!("av2-03: SqliteStore::scan_prefix")
    }

    fn changes(&self, _after: Revision, _limit: usize) -> Result<Changes, StoreError> {
        todo!("av2-03: SqliteStore::changes")
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        todo!("av2-03: SqliteStore::reap_expired")
    }

    fn commit_stats(&self) -> CommitStats {
        todo!("av2-03: SqliteStore::commit_stats")
    }

    fn now(&self) -> StoreTime {
        todo!("av2-03: SqliteStore::now")
    }
}

/// Replaces the state file `dest` with a copy of `backup`.
///
/// # Errors
/// [`SqliteError`].
pub fn restore(_backup: &Path, _dest: &Path) -> Result<u64, SqliteError> {
    todo!("av2-03: restore")
}

#[cfg(test)]
mod tests;
