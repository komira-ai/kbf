//! The in-memory backend. This commit lands the interface; every method is
//! unimplemented so the conformance tests fail for the missing implementation.

#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use crate::clock::{ManualClock, StoreTime};
use crate::conformance::Backend;
use crate::store::Store;
use crate::types::{Changes, Commit, CommitStats, Conflict, Revision, StoreError, Txn, Versioned};

/// A deliberate defect, one per clause the suite checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flaw {
    /// A correct store.
    None,
    /// C1: check and write in separate critical sections.
    NonLinearizableCas,
    /// C2: preconditions judged operation by operation.
    PartialTxn,
    /// C3: counters not durable.
    CounterNotDurable,
    /// C4: expiry judged by a stale clock.
    CallerClockExpiry,
    /// C5: feed passes uncommitted revisions.
    FeedPastPending,
    /// C6: acknowledgement before the sync.
    AckBeforeSync,
    /// C7: a sync per transaction.
    Ungrouped,
}

/// A transaction that has taken its revisions but not committed.
#[derive(Debug)]
pub struct Deferred {
    txn: Txn,
}

/// The in-memory state store.
pub struct Fake {
    clock: Arc<ManualClock>,
    flaw: Flaw,
}

impl Store for Fake {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        todo!("av2-02: Fake::apply_batch")
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        todo!("av2-02: Fake::get")
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        todo!("av2-02: Fake::scan_prefix")
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        todo!("av2-02: Fake::changes")
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        todo!("av2-02: Fake::reap_expired")
    }

    fn commit_stats(&self) -> CommitStats {
        todo!("av2-02: Fake::commit_stats")
    }

    fn now(&self) -> StoreTime {
        todo!("av2-02: Fake::now")
    }
}

/// The suite's factory for the fake, with the flaw to build it with.
#[derive(Clone, Copy, Debug)]
pub struct FakeBackend {
    flaw: Flaw,
}

impl FakeBackend {
    /// A factory for fakes with `flaw`.
    #[must_use]
    pub fn new(flaw: Flaw) -> FakeBackend {
        FakeBackend { flaw }
    }
}

impl Backend for FakeBackend {
    type Store = Fake;

    fn open(&self, clock: Arc<ManualClock>) -> Fake {
        Fake {
            clock,
            flaw: self.flaw,
        }
    }

    fn restart(&self, store: Fake) -> Fake {
        todo!("av2-02: Fake::restart")
    }

    fn commit_out_of_order(
        &self,
        store: &Fake,
        first: Txn,
        second: Txn,
        between: &mut dyn FnMut(),
    ) -> Result<(), Conflict> {
        todo!("av2-02: Fake::commit_out_of_order")
    }
}
