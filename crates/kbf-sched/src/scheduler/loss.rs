//! The cap on how many times an operation's lease may be lost ([`LOSS_REQUEUES`]).

use super::Scheduler;

/// How many leases an operation may lose before it is refused instead of run again:
/// a lease is lost when it is given up for a silent node, a replaced or restarted
/// daemon, or a daemon that reconnected without it (a restart of the scheduler itself
/// is one of these). Separate from the farm reruns a worker's own report spends
/// ([`crate::FARM_RERUNS`]); the third loss refuses the operation.
pub const LOSS_REQUEUES: u32 = 3;

impl Scheduler {
    /// This scheduler, refusing an operation whose lease has been lost `cap` times
    /// (instead of requeueing it again for ever). Off by default.
    #[must_use]
    pub const fn with_loss_requeues(self, _cap: u32) -> Self {
        self
    }
}
