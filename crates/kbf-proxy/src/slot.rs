//! The two child slots, the active-slot handle and the per-stream permit.
//!
//! A stream binds to the slot that was active when its headers arrived, and holds a
//! [`Permit`] on it until its upstream halves are finished (see [`Permit`]). The permit is
//! taken on the slot's own state word, and a retired slot has its `CLOSED` bit set in that
//! same word, so "take a permit" and "slot is not closed" cannot disagree. The retire is a
//! compare-and-swap from "no permits, open" to "closed"; a bind that raced it either
//! incremented first (the retire fails) or sees `CLOSED` (the bind reloads the active slot).
//!
//! **What the permit guarantees, and what it does not.** A permit implies the slot was not
//! closed when it was taken. It does not imply the gate was not holding, or that the slot is
//! still the active one, by the time the stream dispatches: a stream may bind, the supervisor
//! may then hold and quiesce that slot, and the stream may dispatch afterwards (a pool-queued
//! stream does this routinely). That case is expected. Its safety net is the fenced answer and
//! the retry that re-enters [`ActiveSlot::bind`] (see [`crate::replay`]).

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One of the two fixed child slots. Its text form is `"a"` or `"b"`, in `FromStr`,
/// `Display` and serde: the one spelling `update.json` and the status JSON use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    A,
    B,
}

/// A text that is not `"a"` or `"b"`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a slot is \"a\" or \"b\"")]
pub struct SlotParseError;

impl FromStr for Slot {
    type Err = SlotParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "a" => Ok(Self::A),
            "b" => Ok(Self::B),
            _ => Err(SlotParseError),
        }
    }
}

impl std::fmt::Display for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl Slot {
    /// Both slots.
    pub const ALL: [Self; 2] = [Self::A, Self::B];

    /// The stable text form, `"a"` or `"b"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }

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

/// Slot state word: the top bit closes the slot, the next marks it retired (as opposed to
/// never opened), the rest count open permits.
const CLOSED: u64 = 1 << 63;
const RETIRED: u64 = 1 << 62;
const FLAGS: u64 = CLOSED | RETIRED;
/// Gate word: bits 0-1 name the active slot (0 none, 1 A, 2 B), bit 2 is the hold flag.
const HOLD: u8 = 0b100;
const ACTIVE_MASK: u8 = 0b011;

struct Inner {
    gate: AtomicU8,
    slots: [AtomicU64; 2],
    /// Counts the opens of each slot: the key of that slot's upstream connections.
    generations: [AtomicU64; 2],
    /// Counts releases. A held stream samples it before it binds and waits for it to move.
    epoch: AtomicU64,
    /// The bound of the hold in force, in milliseconds.
    hold_max_ms: AtomicU64,
    /// Streams accepted and not yet bound, plus streams that never bind (update API, probes).
    pending: AtomicU64,
}

/// The outcome of [`ActiveSlot::bind`].
#[derive(Debug)]
pub enum Bind {
    /// The stream is bound to this slot until the permit drops.
    Bound(Permit),
    /// The gate is holding: admit the stream to the [`crate::hold::HoldQueue`] with
    /// `hold_max`, wait until [`ActiveSlot::epoch`] differs from `epoch` or the ticket
    /// expires, then bind again. `epoch` was sampled before the gate was read, so a release
    /// that lands after this returns is never missed.
    Held {
        /// The release epoch sampled before the gate.
        epoch: u64,
        /// The bound of the hold in force.
        hold_max: Duration,
    },
    /// No child is active, or the active slot is closed: the stream gets `UNAVAILABLE`.
    NoActive,
}

/// The outcome of [`ActiveSlot::retire`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retire {
    /// The slot was retired (now or before); no stream is bound to it and none can bind.
    Closed,
    /// The slot was never opened: nothing to retire, and not proof that a child is gone.
    NeverOpened,
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

/// [`ActiveSlot::hold`] was called while a hold was in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the gate is already holding")]
pub struct AlreadyHolding;

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
    /// A handle with both slots closed, nothing active and no hold.
    #[must_use]
    pub fn new() -> Self {
        Self::with_gate(0, 0)
    }

    fn with_gate(gate: u8, hold_max_ms: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                gate: AtomicU8::new(gate),
                slots: [AtomicU64::new(CLOSED), AtomicU64::new(CLOSED)],
                generations: [AtomicU64::new(0), AtomicU64::new(0)],
                epoch: AtomicU64::new(0),
                hold_max_ms: AtomicU64::new(hold_max_ms),
                pending: AtomicU64::new(0),
            }),
        }
    }

    /// A handle that is holding from its first instant, for a proxy that starts and must
    /// adopt running children before it serves: streams accepted meanwhile wait up to
    /// `hold_max` instead of failing with `NoActive`. The supervisor ends the hold by
    /// releasing the guard once adoption is done.
    #[must_use]
    pub fn starting(hold_max: Duration) -> (Self, HoldGuard) {
        let ms = u64::try_from(hold_max.as_millis()).unwrap_or(u64::MAX);
        let slots = Self::with_gate(HOLD, ms);
        let guard = HoldGuard {
            inner: Arc::clone(&slots.inner),
        };
        (slots, guard)
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

    /// The release epoch: moves on every release.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }

    /// Opens `slot` for binds (its child is ready) and returns its generation. A slot that
    /// is open already keeps its generation; opening a closed or retired slot starts a new
    /// one, so the upstream connections of the previous child are never reused. A retired
    /// slot can be opened again: slots are reused (A, B, A).
    pub fn open(&self, slot: Slot) -> u64 {
        let word = &self.inner.slots[slot.index()];
        let generation = &self.inner.generations[slot.index()];
        if word.load(Ordering::Acquire) & CLOSED != 0 {
            // Bump before the word opens, so a permit taken after the open sees the new one.
            // Single writer (the supervisor), so the bump cannot be doubled.
            generation.fetch_add(1, Ordering::AcqRel);
            word.store(0, Ordering::Release);
        }
        generation.load(Ordering::Acquire)
    }

    /// The generation of `slot`'s current (or last) open; 0 if never opened.
    #[must_use]
    pub fn generation(&self, slot: Slot) -> u64 {
        self.inner.generations[slot.index()].load(Ordering::Acquire)
    }

    /// Installs adopted children in one call: opens `active` and `other` (if a child runs
    /// there) and makes `active` the active slot. The gate's hold flag is untouched; a
    /// starting proxy is holding, so none of the steps is visible to a stream.
    pub fn adopt(&self, active: Slot, other: Option<Slot>) {
        self.open(active);
        if let Some(slot) = other {
            self.open(slot);
        }
        // `active` was just opened, so the swap cannot be refused.
        let _ = self.set_active(active);
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

    /// Makes no slot active, keeping the hold flag: the old child was stopped and the
    /// candidate failed, so a release must answer `NoActive` at once rather than bind
    /// streams to a dead child and let them wait out the connect timeout.
    pub fn clear_active(&self) {
        self.inner.gate.fetch_and(HOLD, Ordering::AcqRel);
    }

    /// Starts holding new streams for at most `hold_max`. The hold ends when the returned
    /// guard is released or dropped, so a supervisor that panics or is cancelled between the
    /// hold and its release does not leave the gate held. Single writer (the supervisor).
    ///
    /// # Errors
    /// [`AlreadyHolding`]: a hold is in force.
    pub fn hold(&self, hold_max: Duration) -> Result<HoldGuard, AlreadyHolding> {
        let ms = u64::try_from(hold_max.as_millis()).unwrap_or(u64::MAX);
        self.inner.hold_max_ms.store(ms, Ordering::Release);
        self.inner
            .gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |g| {
                (g & HOLD == 0).then_some(g | HOLD)
            })
            .map_err(|_| AlreadyHolding)?;
        Ok(HoldGuard {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Counts a stream from the moment it is accepted, until the guard drops: after its
    /// permit is taken (take the permit first, then drop this), or at the end of a stream
    /// that never binds. [`ActiveSlot::open_streams`] includes it.
    #[must_use]
    pub fn begin_stream(&self) -> Pending {
        self.inner.pending.fetch_add(1, Ordering::AcqRel);
        Pending {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Streams in flight: both slots' permits plus the pending ones. A hint for
    /// restart-on-idle, not a barrier: a connection still in its handshake is not counted,
    /// and the restart race is accepted. Only [`ActiveSlot::retire`] is authoritative.
    #[must_use]
    pub fn open_streams(&self) -> u64 {
        self.open_permits(Slot::A)
            + self.open_permits(Slot::B)
            + self.inner.pending.load(Ordering::Acquire)
    }

    /// Binds a stream whose headers just arrived.
    #[must_use]
    pub fn bind(&self) -> Bind {
        // Before the gate, so a release after the gate read moves the epoch past this one.
        let epoch = self.inner.epoch.load(Ordering::Acquire);
        let mut gate = self.inner.gate.load(Ordering::Acquire);
        loop {
            if gate & HOLD != 0 {
                return Bind::Held {
                    epoch,
                    hold_max: Duration::from_millis(self.inner.hold_max_ms.load(Ordering::Acquire)),
                };
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
                generation: self.generation(slot),
            })
    }

    /// Open permits on `slot`: the proxy's count of streams bound to it, queued ones included.
    #[must_use]
    pub fn open_permits(&self, slot: Slot) -> u64 {
        self.inner.slots[slot.index()].load(Ordering::Acquire) & !FLAGS
    }

    /// Whether [`ActiveSlot::retire`] would close `slot` now: open, not active, no permit.
    /// Read-only, so a supervisor can poll it during bake without latching anything.
    #[must_use]
    pub fn can_retire(&self, slot: Slot) -> bool {
        self.active() != Some(slot) && self.inner.slots[slot.index()].load(Ordering::Acquire) == 0
    }

    /// Closes `slot` if it is not active and has no permit. This is the commit, called after
    /// bake and only then: it is necessary but not sufficient, since the supervisor must also
    /// see the child's own `open_streams` and `open_sessions` at 0. Call from one task (the
    /// supervisor): a concurrent `set_active` to the same slot is not supported.
    #[must_use]
    pub fn retire(&self, slot: Slot) -> Retire {
        if self.active() == Some(slot) {
            return Retire::Current;
        }
        match self.inner.slots[slot.index()].compare_exchange(
            0,
            FLAGS,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Retire::Closed,
            Err(s) if s == CLOSED => Retire::NeverOpened,
            Err(s) if s & CLOSED != 0 => Retire::Closed,
            Err(s) => Retire::Busy(s),
        }
    }
}

/// A hold in force. Releasing or dropping it clears the hold flag, then moves the release
/// epoch (in that order: a waiter that wakes on the epoch must find the gate open).
#[derive(Debug)]
pub struct HoldGuard {
    inner: Arc<Inner>,
}

impl HoldGuard {
    /// Ends the hold now. The held streams then bind, each through [`ActiveSlot::bind`], to
    /// whichever slot is active.
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        self.inner.gate.fetch_and(!HOLD, Ordering::AcqRel);
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

/// A stream counted from accept (see [`ActiveSlot::begin_stream`]).
#[derive(Debug)]
pub struct Pending {
    inner: Arc<Inner>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.inner.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A stream's binding to a slot. Dropping it ends the binding.
///
/// The permit is owned by the future that owns the stream's upstream request and response
/// pumps. It drops only after both upstream halves are finished, reset or dropped, never at
/// the downstream END_STREAM: a retire that saw 0 would otherwise SIGTERM a child that is
/// still serving the request half. A retry drops the old permit only after the old upstream
/// stream is reset, then binds again.
#[derive(Debug)]
pub struct Permit {
    inner: Arc<Inner>,
    slot: Slot,
    generation: u64,
}

impl Permit {
    /// The slot this stream is bound to.
    #[must_use]
    pub const fn slot(&self) -> Slot {
        self.slot
    }

    /// The generation of the slot's open this permit was taken under: the upstream
    /// connections to use are those of `(slot, generation)`.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
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
    use std::sync::atomic::AtomicBool;

    const HOLD_MAX: Duration = Duration::from_secs(20);

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
    fn the_active_slot_is_not_retired_and_a_never_opened_one_says_so() {
        let s = live();
        assert_eq!(s.retire(Slot::A), Retire::Current);
        assert_eq!(s.retire(Slot::B), Retire::NeverOpened);
        s.open(Slot::B);
        assert_eq!(s.retire(Slot::B), Retire::Closed);
        assert_eq!(s.retire(Slot::B), Retire::Closed);
    }

    #[test]
    fn can_retire_is_a_probe_that_latches_nothing() {
        let s = live();
        s.open(Slot::B);
        assert!(!s.can_retire(Slot::A), "active");
        assert!(s.can_retire(Slot::B));
        assert!(s.can_retire(Slot::B), "asking twice changes nothing");
        // Still open: a rollback can make it active.
        s.set_active(Slot::B).expect("still open");
        let held = bound(&s);
        assert!(!s.can_retire(Slot::B), "active and busy");
        s.set_active(Slot::A).expect("A is open");
        assert!(!s.can_retire(Slot::B), "a permit is open");
        drop(held);
        assert!(s.can_retire(Slot::B));
        assert!(!ActiveSlot::new().can_retire(Slot::A), "never opened");
        assert_eq!(s.retire(Slot::B), Retire::Closed);
        assert!(!s.can_retire(Slot::B), "closed already");
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
    fn hold_defers_the_bind_until_the_guard_releases() {
        let s = live();
        let guard = s.hold(HOLD_MAX).expect("first hold");
        assert!(s.holding());
        assert!(matches!(s.hold(HOLD_MAX), Err(AlreadyHolding)));
        assert_eq!(AlreadyHolding.to_string(), "the gate is already holding");
        assert!(matches!(
            s.bind(),
            Bind::Held { hold_max, .. } if hold_max == HOLD_MAX
        ));
        s.open(Slot::B);
        s.set_active(Slot::B).expect("open");
        assert!(s.holding());
        guard.release();
        assert!(!s.holding());
        assert_eq!(bound(&s).slot(), Slot::B);
        assert_eq!(Slot::A.other(), Slot::B);
        assert!(format!("{s:?}").contains("ActiveSlot"));
        assert!(format!("{:?}", bound(&s)).contains("Permit"));
        assert_eq!(ActiveSlot::default().active(), None);
    }

    #[test]
    fn a_release_after_held_is_never_missed() {
        let s = live();
        let guard = s.hold(HOLD_MAX).expect("hold");
        let Bind::Held { epoch, .. } = s.bind() else {
            panic!("holding");
        };
        // The release lands after the stream saw Held and before it parks.
        assert_eq!(s.epoch(), epoch);
        drop(guard);
        assert_ne!(s.epoch(), epoch, "the waiter's condition is already true");
        assert!(!s.holding(), "and the gate is open by the time the epoch moved");
        // A second hold and release moves it again.
        let before = s.epoch();
        s.hold(HOLD_MAX).expect("again").release();
        assert_ne!(s.epoch(), before);
    }

    #[test]
    fn a_dropped_guard_releases_the_gate() {
        let s = live();
        {
            let _guard = s.hold(HOLD_MAX).expect("hold");
            assert!(s.holding());
        }
        assert!(!s.holding());
        assert_eq!(bound(&s).slot(), Slot::A);
    }

    #[test]
    fn a_starting_proxy_holds_until_adoption_is_done() {
        let (s, guard) = ActiveSlot::starting(Duration::from_secs(300));
        assert!(matches!(
            s.bind(),
            Bind::Held { hold_max, .. } if hold_max == Duration::from_secs(300)
        ));
        s.adopt(Slot::B, Some(Slot::A));
        assert_eq!(s.active(), Some(Slot::B));
        assert!(s.holding(), "adoption does not end the hold");
        assert!(s.can_retire(Slot::A), "the other child is open");
        guard.release();
        assert_eq!(bound(&s).slot(), Slot::B);
        let one = ActiveSlot::new();
        one.adopt(Slot::A, None);
        assert_eq!(one.retire(Slot::B), Retire::NeverOpened);
        assert_eq!(bound(&one).slot(), Slot::A);
    }

    #[test]
    fn clearing_the_active_slot_makes_a_release_answer_at_once() {
        let s = live();
        let guard = s.hold(HOLD_MAX).expect("hold");
        s.clear_active();
        assert_eq!(s.active(), None);
        assert!(s.holding(), "the hold survives");
        guard.release();
        assert!(matches!(s.bind(), Bind::NoActive));
    }

    #[test]
    fn a_reopened_slot_gets_a_new_generation_and_its_permits_carry_it() {
        let s = ActiveSlot::new();
        assert_eq!(s.generation(Slot::A), 0);
        assert_eq!(s.open(Slot::A), 1);
        assert_eq!(s.open(Slot::A), 1, "already open: same child");
        s.set_active(Slot::A).expect("open");
        let first = bound(&s);
        assert_eq!(first.generation(), 1);
        s.open(Slot::B);
        s.set_active(Slot::B).expect("open");
        drop(first);
        assert_eq!(s.retire(Slot::A), Retire::Closed);
        // A runs a new child: the pool of generation 1 must not be reused.
        assert_eq!(s.open(Slot::A), 2);
        s.set_active(Slot::A).expect("reopened after a retire");
        assert_eq!(bound(&s).generation(), 2);
        assert_eq!(s.generation(Slot::B), 1);
    }

    #[test]
    fn open_streams_counts_both_slots_and_the_pending() {
        let s = live();
        s.open(Slot::B);
        assert_eq!(s.open_streams(), 0);
        let accepted = s.begin_stream();
        assert_eq!(s.open_streams(), 1, "counted from accept, before any bind");
        let a = bound(&s);
        drop(accepted);
        assert_eq!(s.open_streams(), 1, "the permit takes over");
        s.set_active(Slot::B).expect("open");
        let _b = bound(&s);
        let bypass = s.begin_stream();
        assert_eq!(s.open_streams(), 3, "both slots and a bypass stream");
        drop(a);
        assert_eq!(s.open_streams(), 2);
        assert!(format!("{bypass:?}").contains("Pending"));
    }

    #[test]
    fn a_slot_has_one_text_form() {
        assert_eq!(Slot::A.name(), "a");
        assert_eq!(Slot::B.to_string(), "b");
        assert_eq!("a".parse::<Slot>(), Ok(Slot::A));
        assert_eq!("b".parse::<Slot>(), Ok(Slot::B));
        for bad in ["A", "B", "0", "", "ab", " a"] {
            assert_eq!(bad.parse::<Slot>(), Err(SlotParseError), "{bad:?}");
        }
        assert_eq!(SlotParseError.to_string(), "a slot is \"a\" or \"b\"");
        assert_eq!(serde_json::to_string(&Slot::A).expect("ser"), "\"a\"");
        assert_eq!(serde_json::from_str::<Slot>("\"b\"").expect("de"), Slot::B);
        assert!(serde_json::from_str::<Slot>("\"A\"").is_err());
        assert!(serde_json::from_str::<Slot>("0").is_err());
    }

    #[test]
    fn a_racing_flip_and_retire_never_put_a_permit_on_a_closed_slot() {
        for _ in 0..200 {
            let s = live();
            s.open(Slot::B);
            // Set by the supervisor thread once `retire(A)` has returned `Closed`.
            let closed = Arc::new(AtomicBool::new(false));
            let binder = {
                let (s, closed) = (s.clone(), Arc::clone(&closed));
                std::thread::spawn(move || {
                    let mut late_on_a = 0_u32;
                    for _ in 0..2000 {
                        // Read the flag before the bind: a bind that starts after the
                        // retire completed must never land on A.
                        let was_closed = closed.load(Ordering::Acquire);
                        if let Bind::Bound(p) = s.bind() {
                            late_on_a += u32::from(was_closed && p.slot() == Slot::A);
                        }
                        std::thread::yield_now();
                    }
                    late_on_a
                })
            };
            s.set_active(Slot::B).expect("open");
            while s.retire(Slot::A) != Retire::Closed {
                std::thread::yield_now();
            }
            closed.store(true, Ordering::Release);
            assert_eq!(binder.join().expect("binder"), 0);
            assert_eq!(s.open_permits(Slot::A), 0);
            assert_eq!(bound(&s).slot(), Slot::B);
        }
    }

    #[test]
    fn a_retry_rebinds_through_the_gate_and_waits_for_the_new_child() {
        // A stream bound to A before the hold; the supervisor then held and quiesced A.
        let s = live();
        s.open(Slot::B);
        let first = bound(&s);
        let guard = s.hold(HOLD_MAX).expect("hold");
        // The fenced answer arrives: the retry drops the old permit, then binds again.
        drop(first);
        assert!(matches!(s.bind(), Bind::Held { .. }), "not back onto A");
        s.set_active(Slot::B).expect("open");
        guard.release();
        assert_eq!(bound(&s).slot(), Slot::B);
    }
}
