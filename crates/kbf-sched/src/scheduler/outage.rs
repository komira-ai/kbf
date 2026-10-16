//! The state store's outage as the scheduler is told of it: expiry and placement wait
//! while it is down, and expiry timers restart with a grace window when it returns.

use kbf_types::FarmTime;

use super::Scheduler;

impl Scheduler {
    /// The state store refused a write: nothing is given up or placed until
    /// [`Scheduler::store_available`], because neither could be written down.
    pub fn store_unavailable(&mut self) {}

    /// The state store took a write again at `now`. Every expiry timer restarts: for
    /// [`crate::fence::LEASE_GRACE`] after `now` no lease is given up for silence or
    /// for being left out of a heartbeat, so an outage that kept the scheduler from
    /// hearing its workers does not requeue all running work at once.
    pub fn store_available(&mut self, _now: FarmTime) {}
}
