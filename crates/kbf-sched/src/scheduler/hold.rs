//! Taking back a refusal for want of a worker, when a worker the scheduler does not know
//! could run the operation.
//!
//! A scheduler knows the workers connected to it. Several schedulers may share one state
//! store, each with workers of its own, so an operation no worker of this one can run may
//! be one that a worker of another can. The caller, which reads the store, knows; the
//! scheduler only has to put the operation back.

use std::cmp::Reverse;

use kbf_types::OperationId;

use super::{OpState, Scheduler};
use crate::input::Request;

impl Scheduler {
    /// Takes back the refusal this scheduler has proposed for `operation` because no
    /// worker it holds could run it, if `elsewhere` says that a worker it does not hold
    /// could: the operation goes back in the queue, and the wait that ends in a refusal
    /// starts again from now, so the caller asks again only after another
    /// [`Scheduler::with_unservable_wait`]. Returns whether it did.
    ///
    /// Call it for a refusal that has been proposed (an [`kbf_types::Effect::Commit`] of a
    /// [`kbf_types::ControlRecord::Refusal`]) and not yet fed back as committed. It takes
    /// nothing back from an operation the scheduler does not hold or that is not queued,
    /// from one whose refusal is for its lost leases ([`Scheduler::with_loss_requeues`]),
    /// which no worker elsewhere changes, or when `elsewhere` says no worker could. A
    /// method of the scheduler, not an [`crate::Event`], as [`Scheduler::unplace`] is: the
    /// caller learns of the workers elsewhere from the state store, which no replay of
    /// events shows.
    pub fn hold_refusal(
        &mut self,
        operation: OperationId,
        elsewhere: impl FnOnce(&Request) -> bool,
    ) -> bool {
        let Some(op) = self.ops.get_mut(&operation) else {
            return false;
        };
        if op.state != OpState::Queued {
            return false;
        }
        if op.losses >= self.loss_cap {
            return false;
        }
        if !elsewhere(&op.request) {
            return false;
        }
        if let Some(unservable) = &mut op.unservable {
            unservable.since = self.now;
        }
        let entry = (Reverse(op.request.qos.clone()), operation);
        self.queue.insert(entry);
        true
    }
}
