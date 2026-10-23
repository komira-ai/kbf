//! Forgetting a queued operation that another scheduler of the same state store took.
//!
//! Several schedulers over one state store each queue what the store queues, and each may
//! place it. The store decides which one does: the grant is a conditional write, and the
//! others lose it. A scheduler that learns from the store that an operation it holds queued
//! is no longer queued there (another scheduler granted it, finished it or refused it) drops
//! it, so that it places nothing the store gave to another.

use std::cmp::Reverse;

use kbf_types::{OperationId, WaiterId};

use super::{OpState, Scheduler};

impl Scheduler {
    /// Forgets `operation`, which this scheduler holds queued and another one took: it
    /// leaves the queue and every table that names it, and nothing is recorded (no give-up,
    /// no loss, no fault). Returns the callers it had, for the caller to answer from the
    /// state store when the other scheduler finishes it.
    ///
    /// `None` leaves everything as it was: the scheduler holds no such operation, or it is
    /// not queued (it holds a lease, or it is finished and kept for the finished retention).
    pub fn forget_queued(&mut self, operation: OperationId) -> Option<Vec<WaiterId>> {
        self.ops
            .get(&operation)
            .filter(|op| op.state == OpState::Queued)?;
        let op = self.ops.remove(&operation).expect("checked above");
        self.queue
            .remove(&(Reverse(op.request.qos.clone()), operation));
        if self.in_flight.get(&op.request.key) == Some(&operation) {
            self.in_flight.remove(&op.request.key);
        }
        self.reservations.remove(&operation);
        self.memory.forget(operation);
        self.size_keys.remove(&operation);
        Some(op.waiters)
    }
}
