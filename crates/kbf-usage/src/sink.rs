//! The recorder and the profile store behind one lock.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_types::HostId;
use js_usage::{
    Admitted, Counters, KeyProfile, Limits, LoadIn, MinuteIn, Observed, ProfileCurves,
    ProfileError, ProfileKey, ProfileStore, Recorder, RunSeries,
};
use kbf_state::Store;

use crate::ShapeIndex;

struct Inner {
    recorder: Recorder,
    profiles: ProfileStore,
    node_minutes_discarded: u64,
}

/// What one [`ProfileSink::flush`] did.
#[derive(Debug, PartialEq, Eq)]
pub struct Flushed {
    /// The node minutes taken out of the recorder and dropped.
    pub node_minutes_discarded: usize,
    /// The profiles whose last-used day was written to the store.
    pub touches_persisted: usize,
}

/// What a sink holds and has counted.
#[derive(Debug, PartialEq, Eq)]
pub struct SinkStats {
    /// The keys with a stored profile.
    pub keys: usize,
    /// The bytes of the stored profiles.
    pub bytes: u64,
    /// The stored rows that were not profile rows when the sink opened.
    pub corrupt_rows: usize,
    /// The node minutes dropped by flushes since the sink opened.
    pub node_minutes_discarded: u64,
    /// What the recorder has counted.
    pub recorder: Counters,
}

/// Where measured use is learned and kept: a `js-usage` recorder, which judges a node's
/// readings and hands back each lease minute once, and the profile store, which keeps the
/// runs of every key as rows of the farm's state store.
///
/// One mutex guards both. Store calls ([`run_ended`](Self::run_ended),
/// [`flush`](Self::flush)) run under it and may block on the store, so a caller makes them
/// from a tick on a thread that may block, never from inside the scheduler; the recorder's
/// [`observe`](Self::observe) is memory only.
///
/// The recorder also keeps a node series that kbf does not store. [`flush`](Self::flush)
/// takes the finished minutes out and drops them (counted), so the series does not grow.
/// What the sink restores after a restart is the profiles: a sink opened on the same store
/// has the curves of the one before, and the recorder starts empty (the first reading of a
/// node only sets a baseline).
pub struct ProfileSink {
    inner: Mutex<Inner>,
    curves: Arc<ProfileCurves>,
}

impl ProfileSink {
    /// Opens the profiles in `store` (one scan) and starts an empty recorder. The curves
    /// find a profile through `shapes`. `budget_bytes` bounds the stored rows, and `today`
    /// is the day (days since the Unix epoch) the curves' staleness is judged on.
    ///
    /// # Errors
    /// The scan of the store failed.
    pub fn open(
        store: Arc<dyn Store>,
        shapes: &Arc<ShapeIndex>,
        budget_bytes: u64,
        today: u64,
    ) -> Result<Self, ProfileError> {
        let curves = Arc::new(ProfileCurves::new(shapes.key_map()));
        let profiles = ProfileStore::open(store, Arc::clone(&curves), budget_bytes, today)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                recorder: Recorder::new(),
                profiles,
                node_minutes_discarded: 0,
            }),
            curves,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The curves the scheduler reads (a `CurveSource`).
    #[must_use]
    pub fn curves(&self) -> Arc<ProfileCurves> {
        Arc::clone(&self.curves)
    }

    /// Judges one reading of `host` (see [`Recorder::observe`]) and returns the lease
    /// minutes in it that are new. `epoch` changes when the node's daemon restarts, and
    /// `now_wall_minute` is the farm's wall clock in minutes.
    pub fn observe(
        &self,
        host: &HostId,
        epoch: u64,
        load: &LoadIn,
        limits: Limits,
        now_wall_minute: u64,
    ) -> (Observed, Vec<MinuteIn>) {
        self.lock()
            .recorder
            .observe(host, epoch, load, limits, now_wall_minute)
    }

    /// Adds the run `series` of `key`, which ended on its own on `today`, to the key's
    /// profile and writes the row.
    ///
    /// # Errors
    /// The store failed, another writer changed the row twice, or the row alone does not
    /// fit the budget. A run the profile does not take (too few minutes) is `Ok` with the
    /// reason and writes nothing.
    pub fn run_ended(
        &self,
        key: &ProfileKey,
        series: &RunSeries,
        today: u64,
    ) -> Result<Admitted, ProfileError> {
        self.lock().profiles.run_ended(key, series, today)
    }

    /// Records that `key` was used on `day`, so a key that is placed but idle in runs does
    /// not age out. In memory until the next flush writes it; a key with no stored profile
    /// is ignored.
    pub fn touch(&self, key: &ProfileKey, day: u64) {
        self.lock().profiles.touch(key, day);
    }

    /// The tick's work: drops the node minutes finished before `now_wall_minute`, tells the
    /// curves it is `today`, and writes the last-used day of each key used since its row
    /// was written.
    ///
    /// # Errors
    /// The store failed while writing days. The node minutes were dropped already.
    pub fn flush(&self, now_wall_minute: u64, today: u64) -> Result<Flushed, ProfileError> {
        let mut inner = self.lock();
        let dropped = inner.recorder.node_minutes_due(now_wall_minute).len();
        inner.node_minutes_discarded += dropped as u64;
        self.curves.set_today(today);
        let touches_persisted = inner.profiles.persist_touches()?;
        Ok(Flushed {
            node_minutes_discarded: dropped,
            touches_persisted,
        })
    }

    /// What is stored for `key`: its run count, its fit once it has enough runs, and the
    /// day it was last used; `None` for a key with no row.
    ///
    /// # Errors
    /// The store failed, or the row is not a profile row.
    pub fn profile(&self, key: &ProfileKey) -> Result<Option<KeyProfile>, ProfileError> {
        self.lock().profiles.profile(key)
    }

    /// Forgets a node that left the pool, with its open readings.
    pub fn forget_host(&self, host: &HostId) {
        self.lock().recorder.forget_host(host);
    }

    /// What the sink holds and has counted.
    #[must_use]
    pub fn stats(&self) -> SinkStats {
        let inner = self.lock();
        SinkStats {
            keys: inner.profiles.len(),
            bytes: inner.profiles.total_bytes(),
            corrupt_rows: inner.profiles.corrupt_rows(),
            node_minutes_discarded: inner.node_minutes_discarded,
            recorder: inner.recorder.counters().clone(),
        }
    }
}
