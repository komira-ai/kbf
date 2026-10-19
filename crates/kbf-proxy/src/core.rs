//! The state every stream shares: the gate, the hold queue, the pool, the counters and the
//! in-flight stream count.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::config::{Limits, UpstreamTable};
use crate::control::{ProxySnapshot, SlotStats};
use crate::hold::{HoldLimits, HoldQueue};
use crate::metrics::Metrics;
use crate::pool::{Connector, Pool};
use crate::security::Security;
use crate::slot::{ActiveSlot, Slot};

/// The streams one downstream connection may have open at once. The connection window is this
/// many stream windows, so no set of held bodies can use it up (design note section 4).
pub(crate) const DOWNSTREAM_CONNECTION_STREAMS: u32 = 1000;

/// The clock every stream reads. It is tokio's, so a test under paused time moves it; the
/// hold queue takes `std` instants, which tokio's convert to and from exactly.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

pub(crate) struct Core {
    pub(crate) slots: ActiveSlot,
    pub(crate) queue: HoldQueue,
    pub(crate) pool: Pool,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) limits: Limits,
    pub(crate) security: Security,
    streams: AtomicUsize,
}

impl Core {
    pub(crate) fn new(
        table: UpstreamTable,
        limits: Limits,
        slots: ActiveSlot,
        connector: Connector,
        security: Security,
    ) -> Arc<Self> {
        let metrics = Arc::new(Metrics::default());
        Arc::new(Self {
            slots,
            queue: HoldQueue::new(HoldLimits::default()),
            pool: Pool::new(table, Arc::clone(&metrics), connector),
            metrics,
            limits,
            security,
            streams: AtomicUsize::new(0),
        })
    }

    /// Counts a stream going to a backend (held, dialing or bound) against `max_streams`.
    /// `None`: the limit is reached and the stream is refused at header time.
    pub(crate) fn enter_stream(self: &Arc<Self>) -> Option<StreamCount> {
        self.streams
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.limits.max_streams).then_some(n + 1)
            })
            .ok()
            .map(|_| StreamCount(Arc::clone(self)))
    }

    /// What `GET /v1/update/frontend` reports.
    pub(crate) fn snapshot(&self) -> ProxySnapshot {
        let stats = |slot: Slot| SlotStats {
            open_streams: self.slots.open_permits(slot),
        };
        ProxySnapshot {
            active: self.slots.active(),
            holding: self.slots.holding(),
            held: self.queue.held(),
            per_slot: [stats(Slot::A), stats(Slot::B)],
            // Readiness has one writer, the proxy's own poll of the active child (U4b-3).
            ready: false,
            holds_abandoned: self.slots.abandoned_holds(),
        }
    }

    /// Ends the startup hold into nothing if the supervisor has not released it within
    /// `hold_max`: a supervisor that never arrives is `UNAVAILABLE` and not ready, never a
    /// silent hold.
    pub(crate) async fn abandon_startup_after(&self, hold_max: Duration) {
        tokio::time::sleep(hold_max).await;
        self.slots.abandon_startup();
    }
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("slots", &self.slots)
            .field("held", &self.queue.held())
            .finish_non_exhaustive()
    }
}

/// One stream counted against `max_streams` until it drops.
#[derive(Debug)]
pub(crate) struct StreamCount(Arc<Core>);

impl Drop for StreamCount {
    fn drop(&mut self) {
        self.0.streams.fetch_sub(1, Ordering::AcqRel);
    }
}
