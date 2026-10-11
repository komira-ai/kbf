//! iOS device bookings (`docs/design/ios-devices.md`, section 5.4).
//!
//! A request with `ios.device=1` books one device of its worker, by id, together with
//! its CPU and memory: the first device, by id, that satisfies every device attribute
//! of the request (`kbf_caps::Request::devices`) and that no lease holds. The booking
//! ends when the lease does: its result is accepted, or the lease is given up (its
//! worker silent, or a heartbeat leaving it out). A registration or a resent report
//! that no longer lists the device does not end it, so the device is not booked again
//! while the lease may still be using it.
//!
//! A worker holds at most [`DEVICE_LEASES_PER_WORKER`] device leases at a time: until
//! the sandbox is shown to keep one lease from reaching another lease's device, two
//! device leases on one Mac could each reach both devices.

use crate::scheduler::Worker;

/// How many device leases one worker may hold at a time.
pub const DEVICE_LEASES_PER_WORKER: usize = 1;

impl Worker {
    /// The device a lease asking for `needs` would book on this worker now: the first
    /// by id that satisfies it and is not booked, while the worker holds fewer than
    /// [`DEVICE_LEASES_PER_WORKER`] device leases. `None` if there is none, and for a
    /// request that asks for no device.
    pub(crate) fn free_device(&self, needs: &kbf_caps::Request) -> Option<&str> {
        if self.devices.len() >= DEVICE_LEASES_PER_WORKER {
            return None;
        }
        needs
            .devices(&self.caps)
            .into_iter()
            .find(|id| !self.devices.contains(*id))
    }
}
