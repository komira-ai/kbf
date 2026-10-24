//! A writer whose state store refuses its writes as a superseded handle's: its index rows
//! are dropped.

use std::sync::Arc;

use kbf_history::records::RunOutcome;
use kbf_history::{InvRef, Limits};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};

use crate::support::{Index, Life, Objects, T};

/// A store that reads what the store underneath holds and refuses every write as one of a
/// superseded handle ([`StoreError::Fenced`]).
struct WritesFenced(Arc<dyn Store>);

impl Store for WritesFenced {
    fn apply_batch(&self, _batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Err(StoreError::Fenced)
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
    fn reap_expired(&self) -> Result<usize, StoreError> {
        Err(StoreError::Fenced)
    }
    fn commit_stats(&self) -> CommitStats {
        self.0.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.0.now()
    }
}

/// Asserts that a writer whose store refuses its writes as a superseded handle's drops its
/// index rows as a failed write does: no row lands, the refusal is counted under the index
/// stage, and the rows are not sent again in another form.
#[test]
fn a_superseded_writer_drops_its_rows() {
    let base: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let index = Index::over(Arc::new(WritesFenced(Arc::clone(&base))));
    let objects = Objects::new();
    let mut life = Life::start(&objects, &index, 7, Limits::default());
    let inv = InvRef::named("lost");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    for _ in 0..4 {
        life.step(T + 1);
    }
    assert!(base.scan_prefix("ui/").expect("scan").is_empty());
    assert!(life.writer.drops().index > 0);
}
