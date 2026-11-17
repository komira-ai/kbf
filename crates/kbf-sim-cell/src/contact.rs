//! A simulated daemon's contact with the server: when it last provably reached it, and
//! so when it must fence.
//!
//! The scheduler re-dispatches a node's leases G = 60 s after it last heard from the
//! node. A node that must not run beside a re-dispatched copy stops its work T =
//! [`FENCE_AFTER`] (40 s) after the newest heartbeat the server acknowledged
//! (`docs/design/scheduler.md#fencing-g-and-t`). "After the heartbeat" means after the
//! node *sent* it: the server heard the heartbeat no earlier than it was sent, so
//! counting from the send time keeps the node's deadline inside the server's, whatever
//! the acknowledgement's delay.
//!
//! Contact outlives a stream: leases keep running across a reconnect, and a new
//! stream's acknowledgements renew the same contact. Heartbeat sequence numbers are per
//! stream, so [`Contact::new_stream`] forgets the unacknowledged ones.
//!
//! This module reads no clock: every instant is an argument, the time since the
//! simulation's origin.

use std::collections::BTreeMap;
use std::time::Duration;

/// T: a node fences its leases this long after the newest acknowledged heartbeat
/// (`docs/design/scheduler.md#fencing-g-and-t`). The scheduler re-dispatches after
/// G = 60 s; safety needs T + 5 s < G.
pub const FENCE_AFTER: Duration = Duration::from_secs(40);

/// What one acknowledgement did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acked {
    /// The acknowledged sequence number was one this stream sent and had not yet seen
    /// acknowledged. An unknown or repeated number changes nothing.
    pub known: bool,
    /// The acknowledgement ended a heartbeat gap.
    pub restored: bool,
}

/// A node's view of its contact with the server.
#[derive(Debug)]
pub struct Contact {
    fence_after: Duration,
    gap_after: Option<Duration>,
    /// Send time of the newest message the server has acknowledged.
    confirmed: Option<Duration>,
    /// Send times of this stream's heartbeats that are not yet acknowledged.
    pending: BTreeMap<u64, Duration>,
    in_gap: bool,
}

impl Contact {
    /// A node that has not reached the server yet. `fence_after` is T.
    #[must_use]
    pub fn new(fence_after: Duration) -> Self {
        Self {
            fence_after,
            gap_after: None,
            confirmed: None,
            pending: BTreeMap::new(),
            in_gap: false,
        }
    }

    /// Starts a new stream whose server asked for a heartbeat every `interval`. A gap is
    /// declared when nothing sent in the last two intervals has been acknowledged.
    pub fn new_stream(&mut self, interval: Duration) {
        self.pending.clear();
        self.gap_after = Some(interval.saturating_mul(2));
    }

    /// Records that the server acknowledged a message sent at `sent_at` (the Hello that
    /// a Welcome answers). Returns whether that ended a heartbeat gap.
    pub fn confirm(&mut self, sent_at: Duration) -> bool {
        if self.confirmed.is_none_or(|c| sent_at > c) {
            self.confirmed = Some(sent_at);
        }
        std::mem::replace(&mut self.in_gap, false)
    }

    /// Records that heartbeat `seq` of this stream was sent at `at`.
    pub fn sent(&mut self, seq: u64, at: Duration) {
        self.pending.insert(seq, at);
    }

    /// Records the server's acknowledgement of heartbeat `seq`. An acknowledgement
    /// also covers every earlier heartbeat of the stream.
    pub fn acknowledged(&mut self, seq: u64) -> Acked {
        let Some(&sent_at) = self.pending.get(&seq) else {
            return Acked {
                known: false,
                restored: false,
            };
        };
        self.pending.retain(|&s, _| s > seq);
        let restored = self.confirm(sent_at);
        Acked {
            known: true,
            restored,
        }
    }

    /// When the node must fence its leases: T after the newest acknowledged send.
    /// `None` before the server has acknowledged anything.
    #[must_use]
    pub fn fence_deadline(&self) -> Option<Duration> {
        self.confirmed.map(|c| c + self.fence_after)
    }

    /// Whether contact is lost at `now`: the fence deadline has passed.
    #[must_use]
    pub fn lost(&self, now: Duration) -> bool {
        self.fence_deadline().is_some_and(|d| now >= d)
    }

    /// When a heartbeat gap begins, if one is not already declared.
    #[must_use]
    pub fn gap_deadline(&self) -> Option<Duration> {
        match (self.in_gap, self.confirmed, self.gap_after) {
            (false, Some(c), Some(g)) => Some(c + g),
            _ => None,
        }
    }

    /// Declares a gap if its deadline has passed. Returns how long the server has been
    /// silent when it declares one, once per gap.
    pub fn check_gap(&mut self, now: Duration) -> Option<Duration> {
        let deadline = self.gap_deadline()?;
        if now < deadline {
            return None;
        }
        self.in_gap = true;
        self.confirmed.map(|c| now.saturating_sub(c))
    }
}

/// Whether a lease granted under the lease epoch `granted` is dropped once the newest
/// `Welcome` named `newest`: only when both name one and they differ. A Welcome that
/// names none drops nothing, and a lease granted while none was named is kept.
#[must_use]
pub fn superseded(granted: Option<u64>, newest: Option<u64>) -> bool {
    newest.is_some() && granted.is_some() && granted != newest
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(40);
    const I: Duration = Duration::from_secs(5);
    const T0: Duration = Duration::from_secs(1000);

    fn connected() -> Contact {
        let mut c = Contact::new(T);
        c.new_stream(I);
        c.confirm(T0);
        c
    }

    /// Catches: a fence deadline counted from when the acknowledgement arrived rather
    /// than from when the acknowledged heartbeat was sent, which would move the deadline
    /// past the server's re-dispatch and let two copies of a lease run.
    #[test]
    fn fence_counts_from_the_send_of_the_acknowledged_heartbeat() {
        let mut c = connected();
        let sent = T0 + 2 * I;
        c.sent(1, sent);
        assert!(c.acknowledged(1).known);
        assert_eq!(c.fence_deadline(), Some(sent + T));
        assert!(!c.lost(sent + T - Duration::from_millis(1)));
        assert!(c.lost(sent + T));
    }

    /// Nothing is lost before the server has acknowledged anything.
    #[test]
    fn contact_is_not_lost_before_the_first_acknowledgement() {
        let c = Contact::new(T);
        assert_eq!(c.fence_deadline(), None);
        assert!(!c.lost(Duration::from_secs(10_000)));
        assert_eq!(c.gap_deadline(), None);
    }

    /// An acknowledgement of an older heartbeat, a repeated or unknown sequence number,
    /// or a Welcome for an older send does not move the deadline.
    #[test]
    fn only_newer_acknowledgements_move_the_deadline() {
        let mut c = connected();
        c.sent(1, T0 + I);
        c.sent(2, T0 + 2 * I);
        assert!(c.acknowledged(2).known);
        assert!(!c.acknowledged(1).known, "1 is covered by the ack of 2");
        assert!(!c.acknowledged(9).known, "9 was never sent");
        assert_eq!(c.fence_deadline(), Some(T0 + 2 * I + T));
        c.confirm(T0 + I);
        assert_eq!(c.fence_deadline(), Some(T0 + 2 * I + T));
    }

    /// A new stream keeps contact and drops the old stream's pending heartbeats.
    #[test]
    fn a_new_stream_keeps_contact_and_drops_pending_heartbeats() {
        let mut c = connected();
        c.sent(1, T0 + I);
        c.new_stream(I);
        assert_eq!(c.fence_deadline(), Some(T0 + T));
        assert!(!c.acknowledged(1).known);
    }

    /// A gap is declared once and cleared by the next acknowledgement.
    #[test]
    fn a_gap_is_declared_once_and_cleared_by_an_acknowledgement() {
        let mut c = connected();
        assert_eq!(c.check_gap(T0 + 2 * I - Duration::from_millis(1)), None);
        assert_eq!(c.check_gap(T0 + 2 * I), Some(2 * I));
        assert_eq!(c.gap_deadline(), None, "declared");
        assert_eq!(c.check_gap(T0 + 3 * I), None, "declared once");
        c.sent(7, T0 + 3 * I);
        assert!(c.acknowledged(7).restored);
        assert_eq!(c.gap_deadline(), Some(T0 + 5 * I));
    }

    /// The simulated node's fence time and the scheduler's T are one number. The scheduler
    /// re-dispatches a worker's leases once it has heard nothing from the worker for G, and its
    /// safety argument (`kbf_sched::fence`, `T + margin < G`) takes T from `SELF_FENCE`. Catches
    /// `FENCE_AFTER` changed without `SELF_FENCE`, or the reverse: a node that fences later
    /// runs a lease beside its re-dispatch, and one that fences earlier gives up work the
    /// scheduler still counts as running.
    #[test]
    fn the_fence_time_is_the_schedulers_t() {
        assert_eq!(FENCE_AFTER, kbf_sched::fence::SELF_FENCE);
    }

    /// A lease is superseded only when both epochs are named and differ.
    #[test]
    fn a_lease_is_superseded_only_by_a_different_named_epoch() {
        assert!(superseded(Some(1), Some(2)));
        assert!(!superseded(Some(2), Some(2)));
        assert!(!superseded(Some(1), None));
        assert!(!superseded(None, Some(2)));
        assert!(!superseded(None, None));
    }
}
