//! The state every stream shares: the gate, the hold queue, the pool, the counters and the
//! in-flight stream count.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::config::{Limits, UpstreamTable};
use crate::control::{ProxySnapshot, SlotStats};
use crate::hold::{HoldLimits, HoldQueue};
use crate::metrics::Metrics;
use crate::pool::{Connector, Pool};
use crate::probe::Readiness;
use crate::security::Security;
use crate::slot::{ActiveSlot, Slot};
use crate::term::Terms;
use crate::upstreams::Upstreams;

/// The clock every stream reads. It is tokio's, so a test under paused time moves it; the
/// hold queue takes `std` instants, which tokio's convert to and from exactly.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// How a core finds its children.
pub(crate) enum Mode {
    /// Two slots on a table of ports, one of them active (a supervisor starts and swaps them).
    Slots(UpstreamTable),
    /// A pool of frontends that run on their own.
    Pool(Arc<Upstreams>),
}

pub(crate) struct Core {
    pub(crate) slots: ActiveSlot,
    pub(crate) queue: HoldQueue,
    pub(crate) pool: Pool,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) limits: Limits,
    pub(crate) security: Security,
    /// What `/readyz` says in slot mode: the poll of the active child writes it
    /// (`crate::probe`). In pool mode it is only the question the members' polls ask with.
    pub(crate) ready: Readiness,
    /// The members of the pool, in pool mode; none in slot mode, which is every use of the
    /// gate, the hold queue and the terms.
    pub(crate) upstreams: Option<Arc<Upstreams>>,
    /// The term of each slot's child, for a `WaitExecution` that names the old child's
    /// operation (`crate::term`).
    pub(crate) terms: Terms,
    /// Whether `/ui` is served (the `ui` key of the `[proxy]` table, fixed at start; an atomic
    /// only so a test can switch it on a running rig); off, it is a 404 like any path
    /// outside the table.
    pub(crate) ui: AtomicBool,
    table: UpstreamTable,
    streams: AtomicUsize,
}

impl Core {
    /// A core over fresh metrics: what the tests start from.
    #[cfg(test)]
    pub(crate) fn new(
        table: UpstreamTable,
        limits: Limits,
        slots: ActiveSlot,
        connector: Connector,
        security: Security,
    ) -> Arc<Self> {
        Self::with_metrics(
            Mode::Slots(table),
            limits,
            slots,
            connector,
            security,
            Arc::new(Metrics::default()),
            true,
        )
    }

    /// As [`Core::new`], over `metrics` made earlier: what must be loaded before the secret
    /// is created (the TLS files) counts its failures there. `mode` says where the children
    /// are.
    pub(crate) fn with_metrics(
        mode: Mode,
        limits: Limits,
        slots: ActiveSlot,
        connector: Connector,
        security: Security,
        metrics: Arc<Metrics>,
        ui: bool,
    ) -> Arc<Self> {
        let (table, upstreams) = match mode {
            Mode::Slots(table) => (table, None),
            Mode::Pool(upstreams) => (UpstreamTable::POOL_MODE, Some(upstreams)),
        };
        Arc::new(Self {
            ui: AtomicBool::new(ui),
            slots,
            queue: HoldQueue::new(HoldLimits::default()),
            ready: Readiness::new(connector.clone()),
            terms: Terms::new(connector.clone()),
            upstreams,
            pool: Pool::new(table, Arc::clone(&metrics), connector),
            metrics,
            limits,
            table,
            security,
            streams: AtomicUsize::new(0),
        })
    }

    /// The children's ports.
    pub(crate) const fn table(&self) -> UpstreamTable {
        self.table
    }

    /// Whether `/readyz` answers 200 now: in slot mode, the active child's poll passed; in pool
    /// mode, some member is up and its poll passed.
    pub(crate) fn is_ready(&self) -> bool {
        match &self.upstreams {
            Some(members) => members.is_ready(),
            None => self.ready.is_ready(&self.slots, now()),
        }
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
            ready: self.is_ready(),
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
