//! The two child slots, the active-slot handle and the per-stream permit.
//!
//! A stream binds to the slot that was active when its headers arrived, and holds a
//! [`Permit`] on it until it ends. The bind is one atomic step: the permit is taken on the
//! slot's own state word, and a slot that is retired has its `CLOSED` bit set in that same
//! word, so "take a permit" and "slot is not retired" cannot disagree. The retire is a
//! compare-and-swap from "no permits, open" to "closed"; a bind that raced it either
//! incremented first (the retire fails) or sees `CLOSED` (the bind reloads the active slot).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// One of the two fixed child slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    /// Both slots.
    pub const ALL: [Self; 2] = [Self::A, Self::B];

    /// The other slot.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }

    const fn gate_bits(self) -> u8 {
        match self {
            Self::A => 1,
            Self::B => 2,
        }
    }
}

/// Slot state word: the top bit closes the slot, the rest count open permits.
const CLOSED: u64 = 1 << 63;
/// Gate word: bits 0-1 name the active slot (0 none, 1 A, 2 B), bit 2 is the hold flag.
const HOLD: u8 = 0b100;
const ACTIVE_MASK: u8 = 0b011;

struct Inner {
    gate: AtomicU8,
    slots: [AtomicU64; 2],
}

/// The outcome of [`ActiveSlot::bind`].
#[derive(Debug)]
pub enum Bind {
    /// The stream is bound to this slot until the permit drops.
    Bound(Permit),
    /// The gate is holding: queue the stream (see [`crate::hold`]) and bind on release.
    Held,
    /// No child is active, or the active slot is closed: the stream gets `UNAVAILABLE`.
    NoActive,
}

/// The outcome of [`ActiveSlot::retire`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retire {
    /// The slot is closed; no stream is bound to it and none can bind.
    Closed,
    /// The slot is the active one; swap first.
    Current,
    /// This many permits are still open.
    Busy(u64),
}

/// Why [`ActiveSlot::set_active`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SwapError {
    /// The slot was never opened, or was retired: a swap to it would leave a window with no
    /// active child.
    #[error("slot is closed")]
    Closed,
}

/// The shared handle: which slot is active, whether the gate holds, and each slot's permits.
#[derive(Clone)]
pub struct ActiveSlot {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ActiveSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveSlot")
            .field("active", &self.active())
            .field("holding", &self.holding())
            .finish()
    }
}

impl Default for ActiveSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveSlot {
    /// A handle with both slots closed and nothing active.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                gate: AtomicU8::new(0),
                slots: [AtomicU64::new(CLOSED), AtomicU64::new(CLOSED)],
            }),
        }
    }

    /// The active slot, if any.
    #[must_use]
    pub fn active(&self) -> Option<Slot> {
        Self::decode(self.inner.gate.load(Ordering::Acquire))
    }

    fn decode(gate: u8) -> Option<Slot> {
        Slot::ALL
            .into_iter()
            .find(|slot| gate & ACTIVE_MASK == slot.gate_bits())
    }

    /// Whether new streams are being held.
    #[must_use]
    pub fn holding(&self) -> bool {
        self.inner.gate.load(Ordering::Acquire) & HOLD != 0
    }

    /// Opens `slot` for binds (its child is ready). A slot with open permits is already
    /// open; opening it again changes nothing.
    pub fn open(&self, slot: Slot) {
        let _ = self.inner.slots[slot.index()].compare_exchange(
            CLOSED,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Makes `slot` the active one, atomically, keeping the hold flag.
    ///
    /// # Errors
    /// [`SwapError::Closed`]: the slot is closed.
    pub fn set_active(&self, slot: Slot) -> Result<(), SwapError> {
        if self.inner.slots[slot.index()].load(Ordering::Acquire) & CLOSED != 0 {
            return Err(SwapError::Closed);
        }
        let _ = self
            .inner
            .gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |g| {
                Some((g & HOLD) | slot.gate_bits())
            });
        Ok(())
    }

    /// Starts holding new streams.
    pub fn hold(&self) {
        self.inner.gate.fetch_or(HOLD, Ordering::AcqRel);
    }

    /// Stops holding; the caller then binds each queued stream with [`ActiveSlot::bind`].
    pub fn release(&self) {
        self.inner.gate.fetch_and(!HOLD, Ordering::AcqRel);
    }

    /// Binds a stream whose headers just arrived.
    #[must_use]
    pub fn bind(&self) -> Bind {
        let mut gate = self.inner.gate.load(Ordering::Acquire);
        loop {
            if gate & HOLD != 0 {
                return Bind::Held;
            }
            let Some(slot) = Self::decode(gate) else {
                return Bind::NoActive;
            };
            if let Some(permit) = self.permit(slot) {
                return Bind::Bound(permit);
            }
            // The slot closed under us: the active slot has moved, or never will.
            let next = self.inner.gate.load(Ordering::Acquire);
            if next == gate {
                return Bind::NoActive;
            }
            gate = next;
        }
    }

    fn permit(&self, slot: Slot) -> Option<Permit> {
        self.inner.slots[slot.index()]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |s| {
                (s & CLOSED == 0).then_some(s + 1)
            })
            .ok()
            .map(|_| Permit {
                inner: Arc::clone(&self.inner),
                slot,
            })
    }

    /// Open permits on `slot`: the proxy's count of streams bound to it, queued ones included.
    #[must_use]
    pub fn open_permits(&self, slot: Slot) -> u64 {
        self.inner.slots[slot.index()].load(Ordering::Acquire) & !CLOSED
    }

    /// Closes `slot` if it is not active and has no permit. Call from one task (the
    /// supervisor): a concurrent `set_active` to the same slot is not supported.
    #[must_use]
    pub fn retire(&self, slot: Slot) -> Retire {
        if self.active() == Some(slot) {
            return Retire::Current;
        }
        match self.inner.slots[slot.index()].compare_exchange(
            0,
            CLOSED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Retire::Closed,
            Err(s) if s & CLOSED != 0 => Retire::Closed,
            Err(s) => Retire::Busy(s),
        }
    }
}

/// A stream's binding to a slot. Dropping it ends the binding.
#[derive(Debug)]
pub struct Permit {
    inner: Arc<Inner>,
    slot: Slot,
}

impl Permit {
    /// The slot this stream is bound to.
    #[must_use]
    pub const fn slot(&self) -> Slot {
        self.slot
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Inner")
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.inner.slots[self.slot.index()].fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> ActiveSlot {
        let s = ActiveSlot::new();
        s.open(Slot::A);
        s.set_active(Slot::A).expect("A is open");
        s
    }

    fn bound(s: &ActiveSlot) -> Permit {
        match s.bind() {
            Bind::Bound(p) => p,
            other => panic!("expected a permit, got {other:?}"),
        }
    }

    #[test]
    fn streams_stay_on_the_slot_they_bound_to() {
        let s = live();
        let old: Vec<Permit> = (0..200).map(|_| bound(&s)).collect();
        s.open(Slot::B);
        s.set_active(Slot::B).expect("B is open");
        assert_eq!(bound(&s).slot(), Slot::B);
        assert!(old.iter().all(|p| p.slot() == Slot::A));
        assert_eq!(s.open_permits(Slot::A), 200);
        assert_eq!(s.retire(Slot::A), Retire::Busy(200));
        drop(old);
        assert_eq!(s.retire(Slot::A), Retire::Closed);
        assert_eq!(s.retire(Slot::A), Retire::Closed);
    }

    #[test]
    fn the_active_slot_is_not_retired() {
        let s = live();
        assert_eq!(s.retire(Slot::A), Retire::Current);
        assert_eq!(s.retire(Slot::B), Retire::Closed);
    }

    #[test]
    fn a_closed_slot_cannot_become_active_or_bind() {
        let s = ActiveSlot::new();
        assert_eq!(s.set_active(Slot::B), Err(SwapError::Closed));
        assert!(matches!(s.bind(), Bind::NoActive));
        let t = live();
        t.open(Slot::B);
        t.set_active(Slot::B).expect("open");
        assert_eq!(t.retire(Slot::A), Retire::Closed);
        assert_eq!(t.set_active(Slot::A), Err(SwapError::Closed));
        // A stale active pointer at a closed slot is NoActive, not a spin.
        t.inner.gate.store(Slot::A.gate_bits(), Ordering::Release);
        assert!(matches!(t.bind(), Bind::NoActive));
    }

    #[test]
    fn hold_defers_the_bind_until_release() {
        let s = live();
        s.hold();
        assert!(s.holding());
        assert!(matches!(s.bind(), Bind::Held));
        s.open(Slot::B);
        s.set_active(Slot::B).expect("open");
        assert!(s.holding());
        s.release();
        assert!(!s.holding());
        assert_eq!(bound(&s).slot(), Slot::B);
        assert_eq!(Slot::A.other(), Slot::B);
        assert!(format!("{s:?}").contains("ActiveSlot"));
        assert!(format!("{:?}", bound(&s)).contains("Permit"));
        assert_eq!(ActiveSlot::default().active(), None);
    }

    #[test]
    fn a_racing_flip_and_retire_never_leave_a_permit_on_a_closed_slot() {
        for _ in 0..200 {
            let s = live();
            s.open(Slot::B);
            let binder = {
                let s = s.clone();
                std::thread::spawn(move || {
                    (0..200)
                        .filter_map(|_| match s.bind() {
                            Bind::Bound(p) => Some(p),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
            };
            s.set_active(Slot::B).expect("open");
            let _ = s.retire(Slot::A);
            let permits = binder.join().expect("binder");
            if s.inner.slots[Slot::A.index()].load(Ordering::Acquire) & CLOSED != 0 {
                assert!(permits.iter().all(|p| p.slot() == Slot::B));
            }
        }
    }
}
