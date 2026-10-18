//! The hold gate's bounds and deadline (design section 5 step 6.1 and 5a).
//!
//! While [`crate::slot::ActiveSlot::holding`] is true, a new stream is accepted and its
//! headers read, but nothing is opened to a backend and no flow-control window beyond the
//! initial one is granted. The initial window is [`HoldPolicy::window_bytes`], and it covers
//! headers and data together, so a held stream buffers at most that. The number of held
//! streams is bounded too, so the worst case is `queue_streams * window_bytes`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The bounds of one hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoldPolicy {
    /// Longest a stream is held before it gets `UNAVAILABLE`.
    pub hold_max: Duration,
    /// Most streams held at once; one more gets `UNAVAILABLE` at once.
    pub queue_streams: usize,
    /// Per-stream buffer while held: headers plus data.
    pub window_bytes: u32,
}

impl HoldPolicy {
    /// A frontend swap: 15 s, longer than the 10 s takeover limit.
    #[must_use]
    pub const fn frontend() -> Self {
        Self {
            hold_max: Duration::from_secs(15),
            queue_streams: 2048,
            window_bytes: 64 * 1024,
        }
    }

    /// An all-in-one swap (stop, then start): 300 s.
    #[must_use]
    pub const fn all_in_one() -> Self {
        Self {
            hold_max: Duration::from_secs(300),
            ..Self::frontend()
        }
    }

    /// The most bytes a full queue buffers.
    #[must_use]
    pub const fn worst_case_bytes(&self) -> u64 {
        self.queue_streams as u64 * self.window_bytes as u64
    }
}

/// The queue is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("hold queue is full")]
pub struct QueueFull;

/// The count of held streams (`kbf_proxy_held_streams`) and its bound.
#[derive(Clone, Debug)]
pub struct HoldQueue {
    policy: HoldPolicy,
    held: Arc<AtomicUsize>,
}

impl HoldQueue {
    /// An empty queue under `policy`.
    #[must_use]
    pub fn new(policy: HoldPolicy) -> Self {
        Self {
            policy,
            held: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> HoldPolicy {
        self.policy
    }

    /// Streams held now.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// Admits one stream at `now`. The ticket counts the stream until it drops (released,
    /// expired or reset by the client).
    ///
    /// # Errors
    /// [`QueueFull`]: the bound is reached.
    pub fn admit(&self, now: Instant) -> Result<Ticket, QueueFull> {
        self.held
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.policy.queue_streams).then_some(n + 1)
            })
            .map_err(|_| QueueFull)?;
        Ok(Ticket {
            held: Arc::clone(&self.held),
            deadline: now + self.policy.hold_max,
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
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thousand_held_streams_buffer_at_most_the_window_each_and_expire() {
        let q = HoldQueue::new(HoldPolicy::frontend());
        let t0 = Instant::now();
        let tickets: Vec<Ticket> = (0..1000).map(|_| q.admit(t0).expect("room")).collect();
        assert_eq!(q.held(), 1000);
        assert!(1000 * u64::from(q.policy().window_bytes) <= q.policy().worst_case_bytes());
        assert!(
            tickets
                .iter()
                .all(|t| !t.expired(t0 + Duration::from_secs(14)))
        );
        assert!(
            tickets
                .iter()
                .all(|t| t.expired(t0 + Duration::from_secs(15)))
        );
        assert_eq!(tickets[0].deadline(), t0 + Duration::from_secs(15));
        drop(tickets);
        assert_eq!(q.held(), 0);
    }

    #[test]
    fn the_queue_is_bounded_and_all_in_one_waits_longer() {
        let policy = HoldPolicy {
            queue_streams: 2,
            ..HoldPolicy::all_in_one()
        };
        assert_eq!(policy.hold_max, Duration::from_secs(300));
        let q = HoldQueue::new(policy);
        let now = Instant::now();
        let a = q.admit(now).expect("1");
        let _b = q.admit(now).expect("2");
        assert_eq!(q.admit(now).expect_err("full"), QueueFull);
        drop(a);
        assert!(q.admit(now).is_ok());
    }
}
