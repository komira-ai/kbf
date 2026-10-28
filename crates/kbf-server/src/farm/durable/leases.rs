//! Finding leases in the state store: the `lease-by-op/<operation>` rows, and the leases
//! one worker holds.
//!
//! The `lease/` rows are keyed by lease, so the lease an operation holds is not at a key
//! the operation names. A request for an action that is running joins the operation, and
//! needs its lease: scanning every `lease/` row for it would cost a read of every lease
//! in the farm for each such request. The grant writes the operation's `lease-by-op/` row
//! in its own transaction, beside the lease row, and the end of the lease (a give-up, a
//! result, a refusal) deletes it. The value is the lease id: the term and the sequence,
//! eight bytes each, big-endian.
//!
//! A release that predates the index grants leases with no row of this kind and leaves
//! the row of a lease it ends. So a row that is missing, or that names a lease that is not
//! the operation's, is not taken to mean the operation holds no lease: the lookup then
//! scans the `lease/` rows, as it always did, and a grant puts its row whatever is there.

use super::owner::{pair_of, pair_row};
use super::*;

pub(super) const LEASE_BY_OP: &str = "lease-by-op/";

pub(super) fn lease_by_op_key(operation: OperationId) -> String {
    format!("{LEASE_BY_OP}{:016x}", operation.0)
}

/// The value of the `lease-by-op/` row of `lease`.
pub(super) fn lease_by_op_row(lease: LeaseId) -> [u8; 16] {
    pair_row(lease.term, lease.seq)
}

/// The lease a `lease-by-op/` row names.
fn indexed_lease(key: &str, row: &Versioned) -> Result<LeaseId, StateError> {
    let Some((term, seq)) = pair_of(&row.value) else {
        return Err(corrupt(key, "not a 16-byte lease id"));
    };
    Ok(LeaseId::new(term, seq))
}

impl Durable {
    /// The lease `operation` holds, if it holds one.
    ///
    /// An operation in the queue holds none: the grant takes it out of the queue and puts
    /// the lease row in one transaction. Otherwise the `lease-by-op/` row names the lease,
    /// and the lease row is read to see that it is the operation's; if the row is missing
    /// or does not stand, the `lease/` rows are scanned.
    ///
    /// # Errors
    /// The store cannot be read, or a row read is not one the scheduler wrote.
    pub(super) fn lease_of(
        &self,
        operation: OperationId,
    ) -> Result<Option<LeaseGrant>, StateError> {
        if self.store.get(&queue_key(operation))?.is_some() {
            return Ok(None);
        }
        let index = lease_by_op_key(operation);
        if let Some(row) = self.store.get(&index)? {
            let key = lease_key(indexed_lease(&index, &row)?);
            if let Some(held) = self.store.get(&key)? {
                let grant = self.lease_row(&key, &held)?;
                if grant.operation == operation {
                    return Ok(Some(grant));
                }
            }
        }
        self.scan_lease_of(operation)
    }

    /// The lease `operation` holds, found by reading every `lease/` row.
    fn scan_lease_of(&self, operation: OperationId) -> Result<Option<LeaseGrant>, StateError> {
        let mut lease = None;
        for (key, row) in self.scan_known(LEASE)? {
            let grant = self.lease_row(&key, &row)?;
            if grant.operation == operation {
                lease = Some(grant);
            }
        }
        Ok(lease)
    }

    /// The unfinished operations that hold a lease on `worker`, each as the store holds it.
    /// A lease whose operation has no `op/` row is left out.
    ///
    /// # Errors
    /// The store cannot be read, or a row read is not one the scheduler wrote.
    pub(super) fn held_by(&self, worker: &WorkerId) -> Result<Vec<StoredOperation>, StateError> {
        let mut held = Vec::new();
        for (key, row) in self.scan_known(LEASE)? {
            let grant = self.lease_row(&key, &row)?;
            if grant.worker != *worker {
                continue;
            }
            if let Some(op) = self.stored_with(grant.operation, Some(grant))? {
                held.push(op);
            }
        }
        Ok(held)
    }
}
