//! The swap's [`Gate`] port on `kbf-proxy`'s gate (`docs/design/kbf-proxy-api.md`, section 3).
//!
//! The proxy's `hold` returns a [`HoldGuard`] whose drop, while the hold is in force, ends
//! it **into nothing**: no slot is active and every held and later stream gets
//! `UNAVAILABLE`. The port returns `()`, so the guard lives here, in a field. It is
//! dropped only by [`Gate::release_into`] after the hold ended (then it is inert), by the
//! next `hold`, or with the gate. A swap that stops on a hold it did not release (a crash
//! in a test, a `Halt`) therefore ends the hold the way the design wants a dead supervisor
//! to: into nothing, counted in `kbf_proxy_hold_abandoned_total`.
//!
//! Slots are opened here, at the first `release_into` that names them. The proxy keeps its
//! upstream connections per (slot, generation) and dials when a stream binds, so a slot
//! needs no earlier open; `open` on a slot that [`Gate::retire`] closed returns a new
//! generation, so a respawned child never meets the dead process's pool.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use kbf_proxy::ProxyHandle;
use kbf_proxy::slot::{HoldGuard, Retire, Slot as GateSlot};

use super::model::{PortError, Retired, Slot};
use super::ports::Gate;

pub(super) fn gate_slot(slot: Slot) -> GateSlot {
    match slot {
        Slot::A => GateSlot::A,
        Slot::B => GateSlot::B,
    }
}

fn swap_slot(slot: GateSlot) -> Slot {
    match slot {
        GateSlot::A => Slot::A,
        GateSlot::B => Slot::B,
    }
}

/// A refusal of the proxy's gate, as the port reports it.
fn refused(why: impl std::fmt::Display) -> PortError {
    PortError(why.to_string())
}

/// [`Gate`] over a running proxy.
pub struct ProxyGate {
    pub(super) handle: ProxyHandle,
    /// The hold in force, if the swap took it. Never dropped while its hold is in force
    /// except by the gate itself being dropped.
    pub(super) guard: Mutex<Option<HoldGuard>>,
}

impl ProxyGate {
    pub fn new(handle: ProxyHandle) -> ProxyGate {
        ProxyGate {
            handle,
            guard: Mutex::new(None),
        }
    }

    /// The active slot, as the proxy has it.
    pub fn active(&self) -> Option<Slot> {
        self.handle.slots().active().map(swap_slot)
    }

    /// End the proxy's startup hold with the children found alive: open `active` (and
    /// `other`, if a child runs there) and make `active` the one that serves. Until this
    /// runs a restarted proxy holds new streams, so none is bound to a child that is not
    /// adopted yet; a supervisor that never gets here is ended by the proxy after
    /// `hold_max_s`. Once only: the startup hold is taken from the handle.
    ///
    /// # Errors
    /// The startup hold is gone already (adopted before, or ended by the proxy).
    pub fn adopt(&self, active: Slot, other: Option<Slot>) -> Result<(), PortError> {
        let guard = self
            .handle
            .startup_hold()
            .ok_or_else(|| PortError("the startup hold is gone: adoption already ran".into()))?;
        let slots = self.handle.slots();
        slots.adopt(gate_slot(active), other.map(gate_slot));
        // `adopt` opened `active`, and a closed slot is the only refusal, so this cannot be
        // refused; the guard then drops inert.
        slots.release_into(gate_slot(active)).map_err(refused)?;
        drop(guard);
        Ok(())
    }
}

impl Gate for ProxyGate {
    fn hold(&self, max: Duration) -> Result<(), PortError> {
        let guard = self.handle.slots().hold(max).map_err(refused)?;
        // The previous guard, if any, belongs to a hold that has ended: dropping it does
        // nothing. (`hold` refuses while a hold is in force, so it cannot be a live one.)
        *self.guard.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard);
        Ok(())
    }

    fn release_into(&self, slot: Slot) -> Result<(), PortError> {
        let slots = self.handle.slots();
        let gate_slot = gate_slot(slot);
        slots.open(gate_slot);
        slots.release_into(gate_slot).map_err(refused)?;
        // The hold ended into `slot`: the guard is inert, and dropping it says so.
        drop(
            self.guard
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        Ok(())
    }

    fn expire(&self) -> usize {
        self.handle.expire()
    }

    fn open_streams(&self, slot: Slot) -> u64 {
        self.handle.slots().open_permits(gate_slot(slot))
    }

    fn clear_active(&self) {
        self.handle.slots().clear_active();
    }

    fn retire(&self, slot: Slot) -> Retired {
        match self.handle.slots().retire(gate_slot(slot)) {
            Retire::Closed => Retired::Closed,
            Retire::AlreadyClosed => Retired::AlreadyClosed,
            Retire::NeverOpened => Retired::NeverOpened,
            Retire::Current => Retired::Current,
            Retire::Busy(n) => Retired::Busy(n),
        }
    }
}
