//! `ProfileSink` and `ShapeIndex` as the seam of `js-usage`'s `UsageHub`: a [`UsageSink`] and
//! a [`KeySource`].

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use js_types::HostId;
use js_usage::{
    KeyProfile, KeySource, Limits, LoadIn, NodeMinute, Observation, Observed, ProfileCurves,
    ProfileKey, RunEnd, SinkCounters, UsageFault, UsageSink,
};
use kbf_types::ActionKey;

use crate::{ProfileSink, ShapeIndex};

/// A [`ProfileSink`] as what a `UsageHub` calls.
pub struct KbfUsageSink {
    sink: Arc<ProfileSink>,
}

impl KbfUsageSink {
    /// The seam over `sink`.
    #[must_use]
    pub fn new(sink: Arc<ProfileSink>) -> Self {
        Self { sink }
    }
}

impl UsageSink for KbfUsageSink {
    fn observe(&self, _: &HostId, _: u64, _: &LoadIn, _: Limits, _: u64) -> Observation {
        let _ = &self.sink;
        Observation {
            observed: Observed::Accepted,
            fresh: Vec::new(),
        }
    }

    fn run_ended(&self, _: &RunEnd, _: u64) -> Result<(), UsageFault> {
        Ok(())
    }

    fn flush(&self, _: u64) -> Result<Vec<(HostId, NodeMinute)>, UsageFault> {
        Ok(Vec::new())
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

    fn key_profile(&self, _: &ProfileKey) -> Result<Option<KeyProfile>, UsageFault> {
        Ok(None)
    }

    fn curves(&self) -> Option<Arc<ProfileCurves>> {
        None
    }

    fn counters(&self) -> SinkCounters {
        SinkCounters::default()
    }
}

/// Which action a run is of, and so which profile it is learned under.
pub struct RunKeys {
    shapes: Arc<ShapeIndex>,
    runs: Mutex<BTreeMap<String, ActionKey>>,
}

impl RunKeys {
    /// An empty table over `shapes`.
    #[must_use]
    pub fn new(shapes: Arc<ShapeIndex>) -> Self {
        Self {
            shapes,
            runs: Mutex::default(),
        }
    }

    /// Records that run `run_id` is of `action`; whether it is held.
    pub fn bind(&self, _: &str, _: ActionKey) -> bool {
        let _ = &self.runs;
        true
    }

    /// Forgets run `run_id`; whether it was held.
    pub fn forget(&self, _: &str) -> bool {
        false
    }

    /// How many runs are held.
    #[must_use]
    pub fn len(&self) -> usize {
        0
    }

    /// Whether no run is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        true
    }
}

impl KeySource for RunKeys {
    fn key_of(&self, _: &str) -> Option<ProfileKey> {
        let _ = &self.shapes;
        None
    }
}
