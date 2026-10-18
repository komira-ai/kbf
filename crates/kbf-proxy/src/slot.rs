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
//!
//! **One gate word.** The active slot, the hold flag, the hold's bound and a hold sequence
//! number are bits of one `AtomicU64`, and every change is one compare-and-swap on it, so a
//! stream reads them together and a rejected [`ActiveSlot::hold`] cannot touch a hold in force.
//!
//! **How a hold ends.** [`ActiveSlot::release_into`] ends it into a named slot, atomically.
//! Dropping the [`HoldGuard`] ends it into nothing: the held streams fail `UNAVAILABLE`,
//! because a supervisor that vanished mid-swap cannot say which child is right. Neither
//! guesses a default slot.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::sync::Doorbell;
use crate::sync::atomic::{AtomicU64, Ordering};

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

    const fn gate_bits(self) -> u64 {
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
/// Gate word: bits 0-1 name the active slot (0 none, 1 A, 2 B), bit 2 is the hold flag, bits
/// 8-39 the bound of the hold in force in milliseconds (0 when not holding), bits 40-63 the
/// sequence number of the latest hold (kept after it ends; a guard acts only on its own, and
/// a guard would need 2^24 later holds to alias).
const HOLD: u64 = 0b100;
const ACTIVE_MASK: u64 = 0b011;
/// The sequence number of the hold `ActiveSlot::starting` begins with.
const STARTUP_SEQ: u64 = 1;
const MS_SHIFT: u32 = 8;
const SEQ_SHIFT: u32 = 40;
const SEQ_MASK: u64 = 0xFF_FFFF << SEQ_SHIFT;

fn pack_ms(hold_max: Duration) -> u64 {
    u64::from(u32::try_from(hold_max.as_millis()).unwrap_or(u32::MAX)) << MS_SHIFT
}

const fn seq_of(gate: u64) -> u64 {
    gate >> SEQ_SHIFT
}

struct Inner {
    gate: AtomicU64,
    slots: [AtomicU64; 2],
    /// Counts the opens of each slot: the key of that slot's upstream connections.
    generations: [AtomicU64; 2],
    /// Counts the ends of a hold. A held stream samples it before it binds and waits for it to move.
    epoch: AtomicU64,
    /// The doorbell: mirrors `epoch` for tasks that sleep on it. Written only by `ring`.
    doorbell: Doorbell,
    /// Holds that ended by their guard's drop: nobody released them into a slot.
    abandoned: AtomicU64,
    /// One count of every live `Pending` and every live `Permit`. A stream that holds both
    /// counts twice for an instant; the point is that the count is one word, so a stream that
    /// hands over from one to the other (accept to bind, or a retry) is never missed.
    live: AtomicU64,
}

impl Inner {
    /// Called after the gate changed, by every path that ends a hold: moves the epoch, then
    /// rings the doorbell, so a task that wakes finds the new gate and a new epoch. It lives
    /// here so the guard's drop and `release_into` cannot differ.
    fn ring(&self) {
        // The epoch is taken inside the doorbell's lock: two rings on two threads write the
        // doorbell in the order they took their epochs, so it never goes backwards and a
        // waiter that sampled the older epoch is woken.
        self.doorbell
            .ring(|| self.epoch.fetch_add(1, Ordering::AcqRel) + 1);
    }

    /// Ends the hold `seq` into nothing, if it is still in force: no slot is active, held and
    /// later streams fail, the abandoned count ticks and the doorbell rings. False if the hold
    /// already ended (a release, or an earlier abandon) or `seq` is not the latest.
    fn abandon(&self, seq: u64) -> bool {
        let mine = self
            .gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |g| {
                (g & HOLD != 0 && seq_of(g) == seq).then_some(g & SEQ_MASK)
            })
            .is_ok();
        if mine {
            self.abandoned.fetch_add(1, Ordering::AcqRel);
            self.ring();
        }
        mine
    }
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
    /// This call retired the slot: no stream is bound to it and none can bind. The only
    /// answer a supervisor acts on (it signals the child).
    Closed,
    /// An earlier call retired it and it has not been opened since: nothing to do, signal no
    /// one (a re-run of the retire step after an actor restart lands here).
    AlreadyClosed,
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
        Self::with_gate(0)
    }

    fn with_gate(gate: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                gate: AtomicU64::new(gate),
                slots: [AtomicU64::new(CLOSED), AtomicU64::new(CLOSED)],
                generations: [AtomicU64::new(0), AtomicU64::new(0)],
                epoch: AtomicU64::new(0),
                doorbell: Doorbell::new(),
                abandoned: AtomicU64::new(0),
                live: AtomicU64::new(0),
            }),
        }
    }

    /// A handle that is holding from its first instant, for a proxy that starts and must
    /// adopt running children before it serves: streams accepted meanwhile wait up to
    /// `hold_max` instead of failing with `NoActive`. The supervisor ends the hold with
    /// [`ActiveSlot::release_into`] once adoption is done; the guard dropped first ends it into
    /// nothing.
    #[must_use]
    pub fn starting(hold_max: Duration) -> (Self, HoldGuard) {
        let slots = Self::with_gate(HOLD | pack_ms(hold_max) | (STARTUP_SEQ << SEQ_SHIFT));
        let guard = HoldGuard {
            inner: Arc::clone(&slots.inner),
            seq: STARTUP_SEQ,
        };
        (slots, guard)
    }

    /// Ends the hold [`ActiveSlot::starting`] began, into nothing, if it is still in force:
    /// the proxy's own answer to a supervisor that never arrived (the guard went to the
    /// supervisor, so the proxy cannot drop it). A hold taken later has a later sequence
    /// number and is not touched. Meaningless on a handle that was not built by `starting`.
    #[cfg_attr(kbf_loom, allow(dead_code))]
    pub(crate) fn abandon_startup(&self) -> bool {
        self.inner.abandon(STARTUP_SEQ)
    }

    /// The active slot, if any.
    #[must_use]
    pub fn active(&self) -> Option<Slot> {
        Self::decode(self.inner.gate.load(Ordering::Acquire))
    }

    fn decode(gate: u64) -> Option<Slot> {
        Slot::ALL
            .into_iter()
            .find(|slot| gate & ACTIVE_MASK == slot.gate_bits())
    }

    /// Whether new streams are being held.
    #[must_use]
    pub fn holding(&self) -> bool {
        self.inner.gate.load(Ordering::Acquire) & HOLD != 0
    }

    /// The release epoch: moves each time a hold ends, whichever way.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }

    /// The doorbell a held stream sleeps on: the receiver's value is the epoch, written by the
    /// same code that ends the hold (including a guard's drop on a panic). A waiter parks on
    /// `wait_for(|e| *e != sampled)` and, on every wake, re-reads [`ActiveSlot::epoch`] before
    /// it decides: the atomic is the truth, the channel only wakes the task.
    #[must_use]
    #[cfg(not(kbf_loom))]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.doorbell.subscribe()
    }

    /// Holds that ended by a guard's drop instead of a release into a slot
    /// (`kbf_proxy_hold_abandoned_total`): a supervisor that panicked or was cancelled.
    #[must_use]
    pub fn abandoned_holds(&self) -> u64 {
        self.inner.abandoned.load(Ordering::Acquire)
    }

    /// Opens `slot` for binds (its child is ready) and returns its generation. A slot that
    /// is open already keeps its generation; opening a closed or retired slot starts a new
    /// one, so the upstream connections of the previous child are never reused. A retired
    /// slot can be opened again: slots are reused (A, B, A).
    ///
    /// Respawning a child in an open slot (the slot is not closed) is not an open: the caller
    /// stops the child, `clear_active`s the slot if it is active, polls [`ActiveSlot::retire`]
    /// to `Closed`, and only then opens it again. Skipping the retire leaves the old generation,
    /// and with it the pool of the dead process.
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
                Some((g & !ACTIVE_MASK) | slot.gate_bits())
            });
        Ok(())
    }

    /// Makes no slot active, keeping the hold (and its bound) in force: the child was stopped,
    /// so a release must answer `NoActive` at once rather than bind streams to a dead child
    /// and let them wait out the connect timeout. Also the first step of a respawn.
    pub fn clear_active(&self) {
        self.inner.gate.fetch_and(!ACTIVE_MASK, Ordering::AcqRel);
    }

    /// Makes `slot` the active one and ends the hold, in one compare-and-swap: a stream sees
    /// the old gate (held) or the new one (bound to `slot`), never a mix, and every held
    /// stream then binds into `slot`. With nothing held it only sets the active slot and rings
    /// nothing. It works on any hold, a guard's or an adopted one (`starting`), and into the
    /// old slot after a rollback. A guard of the ended hold becomes inert: its drop does
    /// nothing. Single writer (the supervisor).
    ///
    /// # Errors
    /// [`SwapError::Closed`]: the slot is closed; the gate is unchanged and a hold stays in
    /// force.
    pub fn release_into(&self, slot: Slot) -> Result<(), SwapError> {
        if self.inner.slots[slot.index()].load(Ordering::Acquire) & CLOSED != 0 {
            return Err(SwapError::Closed);
        }
        let before = self
            .inner
            .gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |g| {
                Some((g & SEQ_MASK) | slot.gate_bits())
            })
            .unwrap_or_else(|g| g);
        if before & HOLD != 0 {
            self.inner.ring();
        }
        Ok(())
    }

    /// Starts holding new streams to the backends for at most `hold_max` (the update API and
    /// the probes bypass it; see `route::bypasses_hold`). The bound is stored in the same word
    /// and the same compare-and-swap as the hold flag. Dropping the returned guard before
    /// [`ActiveSlot::release_into`] ends the hold into nothing. Single writer (the supervisor).
    ///
    /// # Errors
    /// [`AlreadyHolding`]: a hold is in force. Nothing changes: the hold in force keeps its
    /// bound and its guard.
    pub fn hold(&self, hold_max: Duration) -> Result<HoldGuard, AlreadyHolding> {
        let before = self
            .inner
            .gate
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |g| {
                (g & HOLD == 0).then_some(
                    (g & ACTIVE_MASK)
                        | HOLD
                        | pack_ms(hold_max)
                        | (((seq_of(g) + 1) << SEQ_SHIFT) & SEQ_MASK),
                )
            })
            .map_err(|_| AlreadyHolding)?;
        Ok(HoldGuard {
            inner: Arc::clone(&self.inner),
            seq: (seq_of(before) + 1) & (SEQ_MASK >> SEQ_SHIFT),
        })
    }

    /// Counts a stream from the moment it is accepted, until the guard drops: after its
    /// permit is taken (take the permit first, then drop this), or at the end of a stream
    /// that never binds. [`ActiveSlot::open_streams`] includes it.
    #[must_use]
    pub fn begin_stream(&self) -> Pending {
        self.inner.live.fetch_add(1, Ordering::AcqRel);
        Pending {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Whether any stream is in flight, as a count: the live `Pending` guards plus the live
    /// permits. A hint for restart-on-idle, not a barrier: a connection still in its handshake
    /// is not counted, and the restart race is accepted. Only [`ActiveSlot::retire`] is
    /// authoritative.
    ///
    /// Read it for `== 0`, not as an exact number: a stream holding a `Pending` and a permit
    /// counts twice. It is one word, and a stream takes its permit before it drops its
    /// `Pending`, and a retry calls [`ActiveSlot::begin_stream`] before it drops its old permit,
    /// so it never reads 0 while a stream lives. (Summing the two slot words and a separate
    /// pending count cannot give that: either read order misses one of the two hand-overs.)
    #[must_use]
    pub fn open_streams(&self) -> u64 {
        self.inner.live.load(Ordering::Acquire)
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
                    hold_max: Duration::from_millis((gate >> MS_SHIFT) & 0xFFFF_FFFF),
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
            .map(|_| {
                // After the permit exists, so no instant has a permit that `live` does not see.
                self.inner.live.fetch_add(1, Ordering::AcqRel);
                Permit {
                    inner: Arc::clone(&self.inner),
                    slot,
                    generation: self.generation(slot),
                }
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
    /// see the child's own `open_streams` and `open_sessions` at 0. A stopped child needs no
    /// bake: poll this until `Closed`, since its sockets are gone and the permits drain.
    /// Call from one task (the
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
            Err(s) if s & CLOSED != 0 => Retire::AlreadyClosed,
            Err(s) => Retire::Busy(s),
        }
    }
}

/// A hold in force. Dropping it while the hold is still in force ends the hold **into
/// nothing**: no slot is active, every held stream and every later one gets `UNAVAILABLE`
/// until the supervisor sets one, and [`ActiveSlot::abandoned_holds`] ticks. The supervisor
/// that wants a slot calls [`ActiveSlot::release_into`] first, which makes the drop inert.
/// This is the consumer's call (U7b), not a default: a guard that released to the active slot
/// on a panic would bind streams to a quiesced child.
#[derive(Debug)]
pub struct HoldGuard {
    inner: Arc<Inner>,
    seq: u64,
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        self.inner.abandon(self.seq);
    }
}

/// A stream counted from accept (see [`ActiveSlot::begin_stream`]).
#[derive(Debug)]
pub struct Pending {
    inner: Arc<Inner>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.inner.live.fetch_sub(1, Ordering::AcqRel);
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
        self.inner.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(all(test, not(kbf_loom)))]
mod tests;

#[cfg(all(test, kbf_loom))]
mod loom_tests;
