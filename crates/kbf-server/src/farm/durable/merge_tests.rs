//! The `est/` rows written read, apply, compare-and-set: two frontends' samples both stay.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use kbf_estimator::{
    Basis, Bounds, Cfg, Day, Instance, Key, Outcome, PlatformClass, Run, Shape, Summaries,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, StoreTime,
    Versioned,
};

use super::*;
use crate::learned::{Sample, Samples};

/// A store over which a second frontend can write between this one's read and its commit.
struct Racing {
    inner: Fake,
    /// Applied just before the next transaction that writes an `est/` row.
    rival: Mutex<Option<Txn>>,
    /// Transactions that write an `est/` row, the rival's excluded.
    est_attempts: AtomicU32,
    /// Every read and write fails.
    down: AtomicBool,
    /// Every transaction that writes an `est/` row is refused for a newer writer.
    fence: AtomicBool,
}

impl Racing {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            rival: Mutex::new(None),
            est_attempts: AtomicU32::new(0),
            down: AtomicBool::new(false),
            fence: AtomicBool::new(false),
        })
    }
}

impl Store for Racing {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.fence.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        let writes_est = batch.iter().flat_map(Txn::ops).any(|op| {
            op.key_cond()
                .is_some_and(|(key, _)| key.starts_with("est/"))
        });
        if writes_est {
            self.est_attempts.fetch_add(1, Ordering::SeqCst);
            let rival = self.rival.lock().expect("lock").take();
            if let Some(txn) = rival {
                let _ = self.inner.txn(txn)?;
            }
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
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

/// One frontend over `store`.
fn frontend(store: &Arc<Racing>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

fn key(cfg: Option<u64>) -> Key {
    Key::new(
        Instance::new("main"),
        PlatformClass::new("linux/x86_64"),
        Basis::Cgroup,
        Shape(7),
        cfg.map(Cfg),
    )
}

/// A successful run that peaked at `peak_mib`.
fn sample(peak_mib: u64) -> Sample {
    Sample {
        run: Run {
            outcome: Outcome::Success,
            wall_us: 2_000_000,
            peak_memory_bytes: Some(peak_mib << 20),
            booked_memory_bytes: 4 << 30,
            raised_memory_bytes: None,
            limit_hits: 0,
            swap_peak_bytes: 0,
            cpu_usage_us: 0,
            node_cpu_pressure_permille: None,
        },
        day: Day(20_000),
        bounds: Bounds::default(),
    }
}

fn samples(key: &Key, peaks_mib: &[u64]) -> Samples {
    (key.clone(), peaks_mib.iter().map(|p| sample(*p)).collect())
}

/// The run of one quiet success of `key`, owed to the store.
pub(super) fn size_samples(key: &Key) -> Samples {
    samples(key, &[256])
}

/// The summary the store holds for `key`.
fn stored(frontend: &Durable, key: &Key) -> Option<kbf_estimator::Summary> {
    let mut held = Summaries::default();
    for (row_key, bytes) in frontend.estimates().expect("read") {
        held.insert_encoded(&row_key, &bytes).expect("decodes");
    }
    held.get(key).cloned()
}

/// The write of a rival frontend that saw `samples`: its summary of `level`, put over
/// whatever the store holds.
fn rival_write(samples: &Samples, level: &Key) -> Txn {
    let mut held = Summaries::default();
    for s in &samples.1 {
        held.observe(&samples.0, &s.run, s.day, &s.bounds);
    }
    let summary = held.get(level).expect("a summary").encode();
    Txn::new().put(
        estimate::row_key(level),
        estimate::encode(&summary),
        Cond::Any,
    )
}

/// Behaviour: two frontends that each saw runs of one key write them one after the other,
/// and the stored summary has the samples of both, at both levels of a configured key,
/// whichever wrote first. Catches: the second write replacing the first frontend's samples.
#[test]
fn two_frontends_samples_of_one_key_both_survive() {
    for order in [[0, 1], [1, 0]] {
        let store = Racing::new();
        let (a, b) = (frontend(&store), frontend(&store));
        let key = key(Some(3));
        let runs = [samples(&key, &[100, 200]), samples(&key, &[900, 300, 400])];
        a.add_samples(&runs[order[0]]).expect("first");
        b.add_samples(&runs[order[1]]).expect("second");
        for level in [key.clone(), key.l2()] {
            let held = stored(&a, &level).expect("a summary");
            assert_eq!(held.samples(), 5, "{level:?}");
        }
        let mut alone = Summaries::default();
        for s in runs.iter().flat_map(|r| &r.1) {
            alone.observe(&key, &s.run, s.day, &s.bounds);
        }
        // The largest of the five samples is in the row, so a frontend that saw only the
        // small ones does not hide it.
        assert_eq!(
            stored(&a, &key).expect("a summary").long_max(),
            alone.get(&key).expect("a summary").long_max()
        );
    }
}

/// Behaviour: a key with no configuration has one row, and it holds both frontends'
/// samples. Catches: the level-2 row written twice in one transaction.
#[test]
fn a_key_without_a_configuration_has_one_row_with_every_sample() {
    let store = Racing::new();
    let (a, b) = (frontend(&store), frontend(&store));
    let key = key(None);
    a.add_samples(&samples(&key, &[100])).expect("first");
    b.add_samples(&samples(&key, &[200, 300])).expect("second");
    assert_eq!(a.estimates().expect("read").len(), 1);
    assert_eq!(stored(&a, &key).expect("a summary").samples(), 3);
}

/// Behaviour: a frontend whose write lost to another frontend's between its read and its
/// commit reads again and applies its runs to what the winner wrote, each run once: the
/// row ends with the winner's sample and this frontend's two, and it took two attempts.
/// Catches: a conflict dropped (the runs lost), or the runs applied to the stale read.
#[test]
fn a_conflicting_write_reads_again_and_applies_each_run_once() {
    let store = Racing::new();
    let a = frontend(&store);
    let key = key(Some(3));
    // The first of A's attempts is preceded by B's row, written after A read none.
    let rival = samples(&key, &[500]);
    *store.rival.lock().expect("lock") = Some(rival_write(&rival, &key));
    a.add_samples(&samples(&key, &[100, 200])).expect("written");
    assert_eq!(store.est_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(stored(&a, &key).expect("a summary").samples(), 3);
}

/// Behaviour: a row the reader cannot decode is replaced by the runs written over it, and
/// the write carries the version read, so a rival's write after that read still conflicts.
/// Catches: an unreadable row that stops the key from ever being written.
#[test]
fn an_unreadable_row_is_replaced_by_the_runs_written_over_it() {
    let store = Racing::new();
    let a = frontend(&store);
    let key = key(None);
    let name = estimate::row_key(&key);
    store
        .txn(Txn::new().put(name, vec![9, 9, 9], Cond::Any))
        .expect("store")
        .expect("put");
    a.add_samples(&samples(&key, &[100, 200])).expect("written");
    assert_eq!(stored(&a, &key).expect("a summary").samples(), 2);
}

/// Behaviour: a store that cannot be read, or one that refuses the write for a newer
/// writer, is an error to the caller, which keeps the runs. Catches: a failed write
/// reported as written (the runs would be dropped).
#[test]
fn a_store_that_cannot_be_read_or_refuses_the_write_is_an_error() {
    let store = Racing::new();
    let a = frontend(&store);
    let key = key(None);
    store.down.store(true, Ordering::SeqCst);
    let error = a.add_samples(&samples(&key, &[100])).expect_err("down");
    assert!(matches!(error, StateError::Store(StoreError::Failed(_))), "{error:?}");
    store.down.store(false, Ordering::SeqCst);
    store.fence.store(true, Ordering::SeqCst);
    let error = a.add_samples(&samples(&key, &[100])).expect_err("fenced");
    assert!(a.superseded_by(&error), "{error:?}");
    assert!(a.estimates().expect("read").is_empty());
}
