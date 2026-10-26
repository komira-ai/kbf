//! The state every stream shares: the members, the pool, the counters and the in-flight stream
//! count.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use crate::config::Limits;
use crate::metrics::Metrics;
use crate::pool::{Connector, Pool};
use crate::probe::Readiness;
use crate::security::Security;
use crate::upstreams::Upstreams;

/// The clock every stream reads. It is tokio's, so a test under paused time moves it.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

pub(crate) struct Core {
    pub(crate) pool: Pool,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) limits: Limits,
    pub(crate) security: Security,
    /// The question the polls of the members ask (`crate::pool_probe`).
    pub(crate) ready: Readiness,
    /// The members of the pool, the frontends the proxy balances over.
    pub(crate) upstreams: Arc<Upstreams>,
    /// Whether `/ui` is served (the `ui` key of the `[proxy]` table, fixed at start; an atomic
    /// only so a test can switch it on a running rig); off, it is a 404 like any path
    /// outside the table.
    pub(crate) ui: AtomicBool,
    streams: AtomicUsize,
    pending: Arc<AtomicU64>,
}

impl Core {
    /// A core over `upstreams`, with the `metrics` made earlier (what must be loaded before the
    /// secret is created, the TLS files, counts its failures there).
    pub(crate) fn new(
        upstreams: Arc<Upstreams>,
        limits: Limits,
        connector: Connector,
        security: Security,
        metrics: Arc<Metrics>,
        ui: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            ui: AtomicBool::new(ui),
            ready: Readiness::new(connector.clone()),
            upstreams,
            pool: Pool::new(Arc::clone(&metrics), connector),
            metrics,
            limits,
            security,
            streams: AtomicUsize::new(0),
            pending: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Whether `/readyz` answers 200 now: some member is up and its poll passed.
    pub(crate) fn is_ready(&self) -> bool {
        self.upstreams.is_ready()
    }

    /// Counts a stream going to a backend (dialing or bound) against `max_streams`. `None`: the
    /// limit is reached and the stream is refused at header time.
    pub(crate) fn enter_stream(self: &Arc<Self>) -> Option<StreamCount> {
        self.streams
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.limits.max_streams).then_some(n + 1)
            })
            .ok()
            .map(|_| StreamCount(Arc::clone(self)))
    }

    /// Counts a stream from the moment it is accepted, until the guard drops: when its answer
    /// is made (a stream the proxy answers itself) or placed on a member (which counts it from
    /// then on).
    pub(crate) fn begin_stream(&self) -> Pending {
        self.pending.fetch_add(1, Ordering::AcqRel);
        Pending(Arc::clone(&self.pending))
    }

    /// The streams in flight: the live [`Pending`] guards plus the streams open on the members.
    /// A hint, read for `== 0`, not as an exact number: a stream placed on a member counts
    /// twice until its [`Pending`] drops. A connection still in its handshake is not counted.
    pub(crate) fn open_streams(&self) -> u64 {
        self.pending.load(Ordering::Acquire) + self.upstreams.open_total()
    }
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("members", &self.upstreams.len())
            .field("open_streams", &self.open_streams())
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

/// A stream counted from accept (see [`Core::begin_stream`]).
#[derive(Debug)]
pub(crate) struct Pending(Arc<AtomicU64>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
