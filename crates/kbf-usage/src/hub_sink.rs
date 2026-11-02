//! `ProfileSink` and `ShapeIndex` as the seam of `js-usage`'s `UsageHub`: a [`UsageSink`] and
//! a [`KeySource`].

use std::collections::BTreeMap;
use std::fmt::Display;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_types::HostId;
use js_usage::{
    Admitted, KeyProfile, KeySource, Limits, LoadIn, NodeMinute, Observation, ProfileCurves,
    ProfileKey, RunEnd, SinkCounters, UsageFault, UsageSink,
};
use kbf_types::ActionKey;

use crate::{MAX_SHAPES, ProfileSink, ShapeIndex};

/// Minutes in a day: the sink's days are the hub's wall minutes over this.
const MINUTES_PER_DAY: u64 = 1440;

fn fault(err: impl Display) -> UsageFault {
    UsageFault(err.to_string())
}

/// A [`ProfileSink`] as what a `UsageHub` calls.
///
/// The hub's minutes are wall minutes; the sink's days are `minute / 1440`. The sink keeps
/// no node series (kbf does not store one), so [`flush`](UsageSink::flush) closes no rows,
/// [`prune`](UsageSink::prune) has nothing to remove and [`node_rows`](UsageSink::node_rows)
/// is empty. A hub's flush is the sink's flush, which drops the finished node minutes
/// (counted in [`SinkStats`](crate::SinkStats)) and writes the keys' last-used days.
pub struct KbfUsageSink {
    sink: Arc<ProfileSink>,
    declined: AtomicU64,
}

impl KbfUsageSink {
    /// The seam over `sink`. The caller keeps its own handle for what the seam does not
    /// carry (`stats`, `forget_host`).
    #[must_use]
    pub fn new(sink: Arc<ProfileSink>) -> Self {
        Self {
            sink,
            declined: AtomicU64::new(0),
        }
    }

    /// How many runs the profile declined: the call succeeded, so the hub counts the run as
    /// learned, but the profile did not take it (no minute at all, or more than a fifth of
    /// the minutes up to its last missing). `runs_learned` minus this is the runs in
    /// profiles.
    #[must_use]
    pub fn declined(&self) -> u64 {
        self.declined.load(Ordering::Relaxed)
    }
}

impl UsageSink for KbfUsageSink {
    fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: Limits,
        now_minute: u64,
    ) -> Observation {
        let (observed, fresh) = self.sink.observe(host, epoch, load, limits, now_minute);
        Observation { observed, fresh }
    }

    fn run_ended(&self, end: &RunEnd, now_minute: u64) -> Result<(), UsageFault> {
        let admitted = self
            .sink
            .run_ended(&end.key, &end.series, now_minute / MINUTES_PER_DAY)
            .map_err(fault)?;
        if admitted != Admitted::Yes {
            self.declined.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    fn flush(&self, now_minute: u64) -> Result<Vec<(HostId, NodeMinute)>, UsageFault> {
        self.sink
            .flush(now_minute, now_minute / MINUTES_PER_DAY)
            .map(|_| Vec::new())
            .map_err(fault)
    }

    fn prune(&self, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn node_rows(
        &self,
        _: &HostId,
        _: u64,
        _: u64,
        _: NonZeroUsize,
    ) -> Result<Vec<NodeMinute>, UsageFault> {
        Ok(Vec::new())
    }

    fn key_profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        self.sink.profile(key).map_err(fault)
    }

    fn curves(&self) -> Option<Arc<ProfileCurves>> {
        Some(self.sink.curves())
    }

    fn counters(&self) -> SinkCounters {
        let counters = self.sink.stats().recorder;
        SinkCounters {
            dropped: counters.dropped,
            minutes_deduped: counters.minutes_deduped,
        }
    }
}

/// Which action a run is of, and so which profile it is learned under.
///
/// The hub asks for a run's key by the run's id, once, when the result is accepted. The
/// farm [`bind`](Self::bind)s a run to its action when the run starts, and
/// [`forget`](Self::forget)s it when the run is over (a run that ends unclean is never
/// asked about, so the hub cannot do it). The profile name comes from the [`ShapeIndex`], which
/// the farm fills when the action is submitted: an action it no longer holds has no key.
///
/// Holds at most [`MAX_SHAPES`] runs; a run bound to a full table is not held and has no
/// key (the hub counts it as a run without one).
pub struct RunKeys {
    shapes: Arc<ShapeIndex>,
    runs: Mutex<BTreeMap<String, ActionKey>>,
    refused: AtomicU64,
}

impl RunKeys {
    /// An empty table over `shapes`.
    #[must_use]
    pub fn new(shapes: Arc<ShapeIndex>) -> Self {
        Self {
            shapes,
            runs: Mutex::default(),
            refused: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, ActionKey>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records that run `run_id` is of `action`; whether it is held. A run already bound
    /// is rebound.
    pub fn bind(&self, run_id: &str, action: ActionKey) -> bool {
        let mut runs = self.lock();
        if runs.len() >= MAX_SHAPES && !runs.contains_key(run_id) {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        runs.insert(run_id.to_owned(), action);
        true
    }

    /// Forgets run `run_id`; whether it was held.
    pub fn forget(&self, run_id: &str) -> bool {
        self.lock().remove(run_id).is_some()
    }

    /// How many binds a full table refused. Each is a run that cannot be learned, so a
    /// table that is full shows here and not only as runs without a key.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// How many runs are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no run is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl KeySource for RunKeys {
    fn key_of(&self, run_id: &str) -> Option<ProfileKey> {
        let action = self.lock().get(run_id)?.clone();
        self.shapes.get(&action)
    }
}
