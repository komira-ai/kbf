//! The one write a superseded holder may still make: rows only it writes.
//!
//! A [`Fenced`](crate::Fenced) handle refuses every write once another handle holds the
//! generation. A process that has been replaced still serves the calls it kept, and some
//! of what those calls leave behind (a run's history rows) is written under keys no
//! other writer can write. [`OwnKeys`] is the handle for those: it routes a transaction
//! whose every operation a [`Permit`] accepts around the generation check, and sends
//! every other transaction through the fenced handle it was made from, so a key the
//! permit does not name can never be written without the check, whatever the caller
//! does. It is made by [`Store::own_keys`](crate::Store::own_keys); the unfenced store
//! beneath is never handed out.

use std::sync::Arc;

use crate::clock::StoreTime;
use crate::store::{LivenessGuard, Store};
use crate::types::{
    Changes, Commit, CommitStats, Cond, Conflict, Op, Revision, ScanPage, ScanRange, StoreError,
    Txn, Versioned,
};

/// What an [`OwnKeys`] handle lets through unchecked: judged one operation at a time, and
/// a transaction goes through only if it accepts all of them. It decides by the key and,
/// for a put, by its precondition and value, so a family that is safe only as an insert
/// is accepted only as one.
pub type Permit = Arc<dyn Fn(&Op) -> bool + Send + Sync>;

/// A store handle that skips the generation check for the writes a [`Permit`] accepts and
/// applies every other write through the fenced handle.
pub struct OwnKeys {
    /// The handle every write the permit does not accept goes through: the fence.
    fenced: Arc<dyn Store>,
    /// The store beneath the fence. Only a transaction the permit accepts reaches it.
    open: Arc<dyn Store>,
    permit: Permit,
}

impl OwnKeys {
    /// A handle that writes what `permit` accepts to `open` and everything else through
    /// `fenced`.
    #[must_use]
    pub fn new(fenced: Arc<dyn Store>, open: Arc<dyn Store>, permit: Permit) -> OwnKeys {
        OwnKeys {
            fenced,
            open,
            permit,
        }
    }

    /// Whether the transaction is made of permitted operations alone (an empty one is
    /// not: nothing names what it writes).
    fn permits(&self, txn: &Txn) -> bool {
        !txn.ops().is_empty() && txn.ops().iter().all(|op| (self.permit)(op))
    }
}

impl Store for OwnKeys {
    /// Applies the permitted transactions of `batch` unchecked and the others through the
    /// fence, in two steps (the unchecked ones first), and returns one result per
    /// transaction in the order of `batch`. A batch that holds both is not atomic as a
    /// whole: an error of the fenced step leaves the unchecked step committed.
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let (mut own, mut rest) = (Vec::new(), Vec::new());
        let routes: Vec<bool> = batch
            .into_iter()
            .map(|txn| {
                let permitted = self.permits(&txn);
                if permitted {
                    own.push(txn);
                } else {
                    rest.push(txn);
                }
                permitted
            })
            .collect();
        let mut own = if own.is_empty() {
            Vec::new()
        } else {
            self.open.apply_batch(own)?
        }
        .into_iter();
        let mut rest = if rest.is_empty() {
            Vec::new()
        } else {
            self.fenced.apply_batch(rest)?
        }
        .into_iter();
        // A backend returns one result per transaction; a missing one is its bug.
        Ok(routes
            .into_iter()
            .map(|permitted| {
                let next = if permitted { own.next() } else { rest.next() };
                next.expect("a backend answers every transaction")
            })
            .collect())
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.fenced.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.fenced.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.fenced.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.fenced.changes(after, limit)
    }

    /// A reap writes the `Expire` rows of keys it does not name, so it is the fence's.
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.fenced.reap_expired()
    }

    fn reap_expired_if(
        &self,
        key: &str,
        cond: Cond,
    ) -> Result<Result<usize, Conflict>, StoreError> {
        self.fenced.reap_expired_if(key, cond)
    }

    fn hold_liveness(&self, generation: u64) -> Result<LivenessGuard, StoreError> {
        self.fenced.hold_liveness(generation)
    }

    fn holder_alive(&self, generation: u64) -> Result<bool, StoreError> {
        self.fenced.holder_alive(generation)
    }

    fn commit_stats(&self) -> CommitStats {
        self.fenced.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.fenced.now()
    }

    fn demote(&self) -> Result<(), StoreError> {
        self.fenced.demote()
    }
}
