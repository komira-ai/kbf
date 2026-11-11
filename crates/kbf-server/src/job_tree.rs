//! The `job-tree/<root sha256 hex>` rows of the frontend's state store: one for each kbf base
//! tree this farm has published (docs/design/actions-as-jobs.md, 3.3, "Old versions").
//!
//! The value is JSON, `{"release":"<commit>","first_published_ms":<ms since the Unix epoch>}`.
//! The row is written by the frontend's start after its publish succeeded, create-only
//! ([`Cond::Absent`]): a root already recorded keeps its row, so the first publish stays first
//! (a rolled-back release republishes its own tree and finds its row). Without a state store
//! (`--state-store`) no row is written: there is nowhere to keep it, and nothing reads it yet.
//!
//! **A new record type, which an older release skips (B8).** The family `job-tree/` is none of
//! the families this binary scans (`op/`, `lease/`, `cordon/`, `cordon-owner/`, ...): every
//! scan is of one prefix and no prefix is a prefix of `job-tree/`, so a reader that does not
//! know the family never reads the row, and an older release's start loads as if it were
//! absent. What an older release loses by skipping it: nothing; it names the trees it did not
//! publish by no row, and the row is a record of publishing, not an input to any decision.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_state::{Cond, Store, StoreError, Txn};
use serde_json::Value;

/// The family of the rows: `job-tree/`.
pub const PREFIX: &str = "job-tree/";

/// The key of the row of the base tree whose root manifest is `root` (64 lowercase hex digits).
#[must_use]
pub fn key(root: &str) -> String {
    format!("{PREFIX}{root}")
}

/// The value of a row: its members in this order, `release` as a JSON string.
fn value(release: &str, first_published_ms: u128) -> Vec<u8> {
    let release = Value::String(release.to_owned());
    format!(r#"{{"release":{release},"first_published_ms":{first_published_ms}}}"#).into_bytes()
}

/// What recording a tree found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// The row was not there and is now.
    First,
    /// The row was there, from an earlier publish, and is as it was.
    Again,
}

/// Why a row could not be written.
#[derive(Debug, thiserror::Error)]
#[error("{key}: {source}")]
pub struct RecordError {
    key: String,
    #[source]
    source: StoreError,
}

/// Records that the release `release` published the base tree `root`, at `now()`, unless a row
/// of the tree exists.
///
/// # Errors
/// [`RecordError`]: the store failed, naming the row's key.
pub fn record(
    store: &dyn Store,
    root: &str,
    release: &str,
    now: fn() -> SystemTime,
) -> Result<Recorded, RecordError> {
    let key = key(root);
    let first_published_ms = now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let value = value(release, first_published_ms);
    match store.txn(Txn::new().put(key.clone(), value, Cond::Absent)) {
        Ok(Ok(_)) => Ok(Recorded::First),
        Ok(Err(_)) => Ok(Recorded::Again),
        Err(source) => Err(RecordError { key, source }),
    }
}

/// [`record`] on the runtime's blocking pool: the store's calls block.
///
/// # Errors
/// As [`record`]; and a store that panics, which is a [`RecordError`] of the failed store too.
pub async fn record_blocking(
    store: Arc<dyn Store>,
    root: String,
    release: String,
    now: fn() -> SystemTime,
) -> Result<Recorded, RecordError> {
    let row = key(&root);
    match tokio::task::spawn_blocking(move || record(&*store, &root, &release, now)).await {
        Ok(done) => done,
        Err(stopped) => Err(RecordError {
            key: row,
            source: StoreError::Failed(format!("the call did not finish: {stopped}")),
        }),
    }
}

#[cfg(test)]
#[path = "job_tree_tests.rs"]
pub(crate) mod tests;
