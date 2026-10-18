//! The hold gate's bounds and deadline (design section 5 step 6.1 and 5a).
//!
//! While [`crate::slot::ActiveSlot::holding`] is true, a new stream is accepted and its
//! headers read, but nothing is opened to a backend and no flow-control window beyond the
//! initial one is granted. The initial window is [`HoldLimits::window_bytes`] of data; the
//! header list (at most [`HEADER_BYTES`]) is not counted against an h2 stream window, so a
//! held stream buffers at most both. The number of held streams is bounded too, so the worst
//! case is [`HoldLimits::worst_case_bytes`].
//!
//! How long a hold lasts is chosen per hold, by the supervisor, in
//! [`crate::slot::ActiveSlot::hold`]: [`crate::config::ProxyConfig::frontend_hold`] for a
//! frontend swap, [`ALL_IN_ONE_HOLD`] for a stop-then-start swap.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The hold of an all-in-one swap (stop, then start).
pub const ALL_IN_ONE_HOLD: Duration = Duration::from_secs(300);
/// The cap on a request's header list, which sits outside the stream window.
pub const HEADER_BYTES: u64 = 16 * 1024;
/// The stream window once a stream is bound and dispatched upstream. A held or still-queued
/// stream stays at [`HoldLimits::window_bytes`]; 64 KB for a whole upload would cap it near
/// 1.3 MB/s at 50 ms.
pub const BOUND_WINDOW_BYTES: u32 = 1024 * 1024;

/// The size bounds of the hold queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldLimits {
    /// Most streams held at once; one more gets `UNAVAILABLE` at once.
    pub queue_streams: usize,
    /// Per-stream data buffer while held.
    pub window_bytes: u32,
}

impl Default for HoldLimits {
    fn default() -> Self {
        Self {
            queue_streams: 2048,
            window_bytes: 64 * 1024,
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

/// The count of held streams (`kbf_proxy_held_streams`) and its bound.
#[derive(Clone, Debug)]
pub struct HoldQueue {
    limits: HoldLimits,
    held: Arc<AtomicUsize>,
}

impl HoldQueue {
    /// An empty queue under `limits`.
    #[must_use]
    pub fn new(limits: HoldLimits) -> Self {
        Self {
            limits,
            held: Arc::new(AtomicUsize::new(0)),
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

    /// Admits one stream at `now`, to be answered `UNAVAILABLE` after `hold_max` (the value
    /// [`crate::slot::Bind::Held`] carried). The ticket counts the stream until it drops (released,
    /// expired or reset by the client).
    ///
    /// # Errors
    /// [`QueueFull`]: the bound is reached.
    pub fn admit(&self, now: Instant, hold_max: Duration) -> Result<Ticket, QueueFull> {
        self.held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.limits.queue_streams).then_some(n + 1)
            })
            .map_err(|_| QueueFull)?;
        Ok(Ticket {
            held: Arc::clone(&self.held),
            deadline: now + hold_max,
        })
    }
}

/// One held stream's place in the queue.
#[derive(Debug)]
pub struct Ticket {
    held: Arc<AtomicUsize>,
    deadline: Instant,
}

impl Ticket {
    /// When the stream must be answered `UNAVAILABLE` if it is still held.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Whether the deadline has passed at `now`.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// What the stream's own task does next. The stream decides once, in its own task, so
    /// release and expiry cannot both win: a release that is seen binds the stream even when
    /// the deadline has passed too, and an expired stream is never bound afterwards.
    #[must_use]
    pub fn wake(&self, now: Instant, released: bool) -> Wake {
        if released {
            Wake::Bind
        } else if self.expired(now) {
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
        self.held.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOLD: Duration = Duration::from_secs(20);

    #[test]
    fn a_thousand_held_streams_expire_and_the_worst_case_is_window_plus_headers() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        let tickets: Vec<Ticket> = (0..1000).map(|_| q.admit(t0, HOLD).expect("room")).collect();
        assert_eq!(q.held(), 1000);
        // 2048 streams of (64 KiB window + 16 KiB header list).
        assert_eq!(q.limits().worst_case_bytes(), 2048 * (65_536 + 16_384));
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
        let a = q.admit(now, ALL_IN_ONE_HOLD).expect("1");
        assert_eq!(a.deadline(), now + Duration::from_secs(300));
        let _b = q.admit(now, HOLD).expect("2");
        assert_eq!(q.admit(now, HOLD).expect_err("full"), QueueFull);
        drop(a);
        assert!(q.admit(now, HOLD).is_ok());
        const { assert!(BOUND_WINDOW_BYTES > 64 * 1024) };
    }

    #[test]
    fn release_wins_over_expiry_when_both_are_seen() {
        let q = HoldQueue::new(HoldLimits::default());
        let t0 = Instant::now();
        let t = q.admit(t0, HOLD).expect("room");
        assert_eq!(t.wake(t0, false), Wake::Wait);
        assert_eq!(t.wake(t0 + HOLD, false), Wake::Expire);
        assert_eq!(t.wake(t0, true), Wake::Bind);
        // Both at once: one winner, and it is the bind (the gate did release).
        assert_eq!(t.wake(t0 + HOLD, true), Wake::Bind);
    }
}
