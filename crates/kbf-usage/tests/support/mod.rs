//! What the tests of this crate share: keys, readings, and stores.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use js_types::{HostId, LeaseId};
use js_usage::{LoadIn, MinuteIn, ProfileKey, RunMinute, RunSeries};
use kbf_estimator::{Basis, Cfg, Instance, Key, PlatformClass, Shape};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction};

pub fn key(instance: &str, platform: &str, basis: Basis, shape: u64, cfg: Option<u64>) -> Key {
    Key::new(
        Instance::new(instance),
        PlatformClass::new(platform),
        basis,
        Shape(shape),
        cfg.map(Cfg),
    )
}

/// A key whose parts are all different from `other()`'s.
pub fn base_key() -> Key {
    key("main", "linux-x86_64", Basis::Cgroup, 7, Some(9))
}

pub fn profile(text: &str) -> ProfileKey {
    ProfileKey::parse(text).expect("a valid profile key")
}

pub fn digest(tag: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [tag; 32], 100 + u64::from(tag))
}

pub fn action(instance: &str, tag: u8) -> ActionKey {
    ActionKey {
        instance: instance.to_owned(),
        action: digest(tag),
    }
}

pub fn host(name: &str) -> HostId {
    HostId::new(name).expect("a valid host id")
}

pub fn lease(seq: u64) -> LeaseId {
    LeaseId::new(1, seq)
}

/// A reading with the counters of the `n`th beat, one second and half a core apart.
pub fn beat(n: u64, minutes: Vec<MinuteIn>) -> LoadIn {
    LoadIn {
        seq: n,
        mono_usec: n * 1_000_000,
        cpu_usage_usec: n * 500_000,
        working_set_bytes: 1 << 20,
        actions_cpu_limited: false,
        covered: Vec::new(),
        minutes,
        clocks: Vec::new(),
    }
}

pub fn minute_of(lease: LeaseId, minute: u16, cores: u32) -> MinuteIn {
    MinuteIn {
        lease,
        minute,
        mean_cores_milli: cores,
        peak_window_milli: cores + 1,
        working_set_max_bytes: 5_000,
    }
}

/// A run of `minutes` minutes, each at 1000 millicores and 5000 bytes.
pub fn run_of(minutes: u32) -> RunSeries {
    RunSeries {
        minutes: (0..minutes)
            .map(|minute| RunMinute {
                minute,
                cores_milli: 1_000,
                ws_bytes: 5_000,
            })
            .collect(),
        flags: 0,
    }
}

pub fn fake() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

pub fn put(store: &dyn Store, key: &str, value: &[u8]) {
    store
        .txn(Txn::new().put(key, value, Cond::Any))
        .expect("the store is reachable")
        .expect("an unconditional put commits");
}

/// A store whose reads of rows and writes fail while it is down, and that is the fake
/// underneath otherwise.
pub struct Flaky {
    inner: Arc<Fake>,
    down: AtomicBool,
}

impl Flaky {
    pub fn over(inner: Arc<Fake>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            down: AtomicBool::new(false),
        })
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("down for the test".to_owned()));
        }
        Ok(())
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.check()?;
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.check()?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.check()?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}
