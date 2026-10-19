//! The hold gate's bounds and deadline (design section 5 step 6.1 and 5a).
//!
//! While [`crate::slot::ActiveSlot::holding`] is true, a new stream to a backend is accepted
//! and its headers read, but nothing is opened upstream and no credit is returned to the
//! client. h2 0.4.20 has one initial window for every stream of a connection and no
//! per-stream setter, so a held stream may receive at most [`STREAM_WINDOW_BYTES`] of data,
//! the same window a bound stream has; the header list (at most [`HEADER_BYTES`]) is not
//! counted against an h2 stream window. The number of held streams is bounded too, so the
//! worst case is [`HoldLimits::worst_case_bytes`].
//!
//! How long a hold lasts is chosen per hold, by the supervisor, in
//! [`crate::slot::ActiveSlot::hold`]: [`crate::config::ProxyConfig::frontend_hold`] for a
//! frontend swap, [`ALL_IN_ONE_HOLD`] for a stop-then-start swap. Each held stream carries its
//! own deadline (a [`Ticket`]); [`HoldQueue::expire`] is the explicit sweep that fails the
//! streams past it now and counts them, and the stream's own timer is the guarantee when
//! nobody sweeps. Whichever acts first wins, once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The hold of an all-in-one swap (stop, then start).
pub const ALL_IN_ONE_HOLD: Duration = Duration::from_secs(300);
/// The cap on a request's header list, which sits outside the stream window.
pub const HEADER_BYTES: u64 = 16 * 1024;
/// The one initial h2 window of every stream, held or bound, in both directions (h2 0.4.20
/// cannot raise one stream's window after the fact). A single upload is capped near
/// `window / RTT`: 5 MB/s at 50 ms, 250 MB/s at 1 ms.
pub const STREAM_WINDOW_BYTES: u32 = 256 * 1024;

/// The size bounds of the hold queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldLimits {
    /// Most streams held at once; one more gets `UNAVAILABLE` at once.
    pub queue_streams: usize,
    /// The data a held stream may buffer: the connection's one stream window.
    pub window_bytes: u32,
}

impl Default for HoldLimits {
    fn default() -> Self {
        Self {
            queue_streams: 2048,
            window_bytes: STREAM_WINDOW_BYTES,
        }
    }
}

impl HoldLimits {
    /// The most bytes a full queue buffers: window plus header list per stream.
    #[must_use]
    pub const fn worst_case_bytes(&self) -> u64 {
        self.queue_streams as u64 * (self.window_bytes as u64 + HEADER_BYTES)
    }
}

/// The queue is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("hold queue is full")]
pub struct QueueFull;

/// A held stream's state, decided once: `WAITING` to `BOUND` (a release was seen) or to
/// `EXPIRED` (the sweep or the stream's own deadline), never both.
const WAITING: u8 = 0;
const BOUND: u8 = 1;
const EXPIRED: u8 = 2;

#[derive(Debug)]
struct Cell {
    deadline: Instant,
    state: AtomicU8,
}

#[derive(Debug, Default)]
struct Shared {
    /// Held streams. The registry's lock is taken only on the held path (admit, drop and the
    /// sweep), never by a bound stream.
    cells: Mutex<HashMap<u64, Arc<Cell>>>,
    next_id: AtomicU64,
    expired_total: AtomicU64,
}

/// The held streams (`kbf_proxy_held_streams`), their bound and their deadlines.
#[derive(Clone, Debug)]
pub struct HoldQueue {
    limits: HoldLimits,
    held: Arc<AtomicUsize>,
    shared: Arc<Shared>,
}

impl HoldQueue {
    /// An empty queue under `limits`.
    #[must_use]
    pub fn new(limits: HoldLimits) -> Self {
        Self {
            limits,
            held: Arc::new(AtomicUsize::new(0)),
            shared: Arc::new(Shared::default()),
        }
    }

    /// The limits in force.
    #[must_use]
    pub const fn limits(&self) -> HoldLimits {
        self.limits
    }

    /// Streams held now.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// Streams failed by a deadline so far (`kbf_proxy_hold_expired_total`): each counted once,
    /// by the sweep or by the stream's own timer.
    #[must_use]
    pub fn expired_total(&self) -> u64 {
        self.shared.expired_total.load(Ordering::Acquire)
    }

    /// Admits one stream, to be answered `UNAVAILABLE` `hold_max` (the value
    /// [`crate::slot::Bind::Held`] carried) after `since`: `None` for a stream's first hold,
    /// and for a stream that is held again after a release (a retry, or a second hold) the
    /// [`Ticket::since`] of its first ticket, so chained holds cannot keep one stream longer
    /// than one bound. The ticket counts the stream until it drops (released, expired or reset
    /// by the client).
    ///
    /// # Errors
    /// [`QueueFull`]: the bound is reached.
    pub fn admit(
        &self,
        now: Instant,
        hold_max: Duration,
        since: Option<Instant>,
    ) -> Result<Ticket, QueueFull> {
        self.held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.limits.queue_streams).then_some(n + 1)
            })
            .map_err(|_| QueueFull)?;
        let since = since.unwrap_or(now);
        let cell = Arc::new(Cell {
            deadline: since + hold_max,
            state: AtomicU8::new(WAITING),
        });
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.shared
            .cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, Arc::clone(&cell));
        Ok(Ticket {
            held: Arc::clone(&self.held),
            shared: Arc::clone(&self.shared),
            id,
            since,
            cell,
        })
    }

    /// The sweep: fails every held stream whose deadline is at or before `now` and that is not
    /// yet decided, counts them in [`HoldQueue::expired_total`] and returns how many this call
    /// failed. A stream that already saw a release is not touched; a stream this call failed
    /// answers `UNAVAILABLE` at its next wake even if a release follows. The supervisor calls
    /// it on each poll, through `ProxyHandle::expire`.
    pub fn expire(&self, now: Instant) -> usize {
        let cells = self
            .shared
            .cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut failed = 0;
        for cell in cells.values() {
            if now >= cell.deadline && cell.fail() {
                failed += 1;
            }
        }
        drop(cells);
        self.shared
            .expired_total
            .fetch_add(failed as u64, Ordering::AcqRel);
        failed
    }
}

impl Cell {
    /// Waiting to expired. True if this call made the change.
    fn fail(&self) -> bool {
        self.state
            .compare_exchange(WAITING, EXPIRED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// One held stream's place in the queue.
#[derive(Debug)]
pub struct Ticket {
    held: Arc<AtomicUsize>,
    shared: Arc<Shared>,
    id: u64,
    since: Instant,
    cell: Arc<Cell>,
}

impl Ticket {
    /// When the stream must be answered `UNAVAILABLE` if it is still held.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.cell.deadline
    }

    /// When this stream was first held: pass it to [`HoldQueue::admit`] if the stream is held
    /// again.
    #[must_use]
    pub const fn since(&self) -> Instant {
        self.since
    }

    /// Whether the deadline has passed at `now`.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.cell.deadline
    }

    /// What the stream's own task does next. The decision is one compare-and-swap on the
    /// ticket, shared with [`HoldQueue::expire`], so release and expiry cannot both win: a
    /// release that is seen binds the stream even when the deadline has passed too, unless the
    /// sweep already failed it, and an expired stream is never bound afterwards.
    #[must_use]
    pub fn wake(&self, now: Instant, released: bool) -> Wake {
        let state = &self.cell.state;
        if released {
            return match state.compare_exchange(WAITING, BOUND, Ordering::AcqRel, Ordering::Acquire)
            {
                Err(EXPIRED) => Wake::Expire,
                _ => Wake::Bind,
            };
        }
        if state.load(Ordering::Acquire) == EXPIRED {
            Wake::Expire
        } else if self.expired(now) {
            // Counted only if this call is the one that failed it (the sweep may have).
            let failed = u64::from(self.cell.fail());
            self.shared
                .expired_total
                .fetch_add(failed, Ordering::AcqRel);
            Wake::Expire
        } else {
            Wake::Wait
        }
    }
}

/// The one decision a held stream takes per wakeup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Still held: wait for the release epoch to change or the deadline.
    Wait,
    /// The gate released: bind again through [`crate::slot::ActiveSlot::bind`].
    Bind,
    /// The deadline passed: answer `UNAVAILABLE`.
    Expire,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.shared
            .cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
        self.held.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: Duration = Duration::from_secs(20);

    fn admit(q: &HoldQueue, now: Instant) -> Ticket {
        q.admit(now, HOLD, None).expect("room")
    }

    #[test]
    fn a_thousand_held_streams_expire_and_the_worst_case_is_window_plus_headers() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        let tickets: Vec<Ticket> = (0..1000).map(|_| admit(&q, t0)).collect();
        assert_eq!(q.held(), 1000);
        // 2048 streams of (256 KiB window + 16 KiB header list).
        assert_eq!(q.limits().worst_case_bytes(), 2048 * (262_144 + 16_384));
        assert!(
            tickets
                .iter()
                .all(|t| !t.expired(t0 + Duration::from_secs(19)))
        );
        assert!(
            tickets
                .iter()
                .all(|t| t.expired(t0 + Duration::from_secs(20)))
        );
        assert_eq!(tickets[0].deadline(), t0 + HOLD);
        drop(tickets);
        assert_eq!(q.held(), 0);
    }

    #[test]
    fn the_queue_is_bounded_and_the_deadline_is_chosen_per_hold() {
        let q = HoldQueue::new(HoldLimits {
            queue_streams: 2,
            ..HoldLimits::default()
        });
        let now = Instant::now();
        let a = q.admit(now, ALL_IN_ONE_HOLD, None).expect("1");
        assert_eq!(a.deadline(), now + Duration::from_secs(300));
        let _b = admit(&q, now);
        assert_eq!(q.admit(now, HOLD, None).expect_err("full"), QueueFull);
        drop(a);
        assert!(q.admit(now, HOLD, None).is_ok());
        assert_eq!(STREAM_WINDOW_BYTES, 256 * 1024);
    }

    #[test]
    fn release_wins_over_expiry_when_both_are_seen() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        let t = admit(&q, t0);
        assert_eq!(t.wake(t0, false), Wake::Wait);
        assert_eq!(t.wake(t0 + HOLD, true), Wake::Bind, "release seen first");
        assert_eq!(
            q.expired_total(),
            0,
            "a bound stream is never counted expired"
        );
        let t2 = admit(&q, t0);
        assert_eq!(t2.wake(t0, true), Wake::Bind);
        // The ticket's own timer: a stream that sees only the deadline expires, once.
        let t3 = admit(&q, t0);
        assert_eq!(t3.wake(t0 + HOLD, false), Wake::Expire);
        assert_eq!(t3.wake(t0 + HOLD, false), Wake::Expire);
        assert_eq!(q.expired_total(), 1, "counted once, not per wake");
    }

    #[test]
    fn the_sweep_fails_streams_past_the_bound_counts_them_and_returns_the_count() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        let early: Vec<Ticket> = (0..3).map(|_| admit(&q, t0)).collect();
        let late = q
            .admit(t0 + Duration::from_secs(10), HOLD, None)
            .expect("room");
        assert_eq!(
            q.expire(t0 + Duration::from_secs(19)),
            0,
            "nothing is past it"
        );
        assert_eq!(q.expire(t0 + HOLD), 3, "the three early streams");
        assert_eq!(
            q.expire(t0 + HOLD),
            0,
            "a second sweep counts nothing again"
        );
        assert_eq!(q.expired_total(), 3);
        // The late stream is still within its bound and is not touched.
        assert_eq!(late.wake(t0 + HOLD, false), Wake::Wait);
        // Swept streams stay failed even if a release follows: one winner.
        assert!(
            early
                .iter()
                .all(|t| t.wake(t0 + HOLD, true) == Wake::Expire)
        );
        assert_eq!(late.wake(t0 + HOLD, true), Wake::Bind);
        assert_eq!(
            q.expire(t0 + Duration::from_secs(40)),
            0,
            "a bound stream is not failed"
        );
        assert_eq!(q.expired_total(), 3);
        // A stream that expired by its own timer is not counted a second time by the sweep.
        let own = admit(&q, t0);
        assert_eq!(own.wake(t0 + HOLD, false), Wake::Expire);
        assert_eq!(q.expire(t0 + HOLD), 0);
        assert_eq!(q.expired_total(), 4);
    }

    #[test]
    fn a_stream_held_again_keeps_its_first_deadline() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        let first = admit(&q, t0);
        let since = first.since();
        drop(first);
        // Released at +15 s, held again by the next hold, whose bound is 300 s.
        let again = q
            .admit(t0 + Duration::from_secs(15), ALL_IN_ONE_HOLD, Some(since))
            .expect("room");
        assert_eq!(
            again.deadline(),
            t0 + ALL_IN_ONE_HOLD,
            "since the first hold, not the second"
        );
        let fresh = q
            .admit(t0 + Duration::from_secs(15), ALL_IN_ONE_HOLD, None)
            .expect("room");
        assert_eq!(fresh.deadline(), t0 + Duration::from_secs(315));
    }

    #[test]
    fn a_dropped_ticket_leaves_the_registry() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        drop(admit(&q, t0));
        assert_eq!(q.held(), 0);
        assert_eq!(
            q.expire(t0 + HOLD),
            0,
            "a stream the client reset is not counted"
        );
    }
}
