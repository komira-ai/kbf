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
    /// Applied, one before each transaction that writes an `est/` row, in order.
    rivals: Mutex<Vec<Txn>>,
    /// Reads that were asked of the store: `get` and `scan_prefix` calls.
    reads: AtomicU32,
    /// The transaction that writes an `est/` row after this many have been tried fails.
    fail_after: AtomicU32,
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
            rivals: Mutex::new(Vec::new()),
            reads: AtomicU32::new(0),
            fail_after: AtomicU32::new(u32::MAX),
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
            let tried = self.est_attempts.fetch_add(1, Ordering::SeqCst);
            if tried >= self.fail_after.load(Ordering::SeqCst) {
                return Err(StoreError::Failed("the store failed".to_owned()));
            }
            let mut rivals = self.rivals.lock().expect("lock");
            let rival = (!rivals.is_empty()).then(|| rivals.remove(0));
            drop(rivals);
            if let Some(txn) = rival {
                let _ = self.inner.txn(txn)?;
            }
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
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
        a.add_samples(std::slice::from_ref(&runs[order[0]])).expect("first");
        b.add_samples(std::slice::from_ref(&runs[order[1]])).expect("second");
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
    a.add_samples(std::slice::from_ref(&samples(&key, &[100]))).expect("first");
    b.add_samples(std::slice::from_ref(&samples(&key, &[200, 300]))).expect("second");
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
    store.rivals.lock().expect("lock").push(rival_write(&rival, &key));
    a.add_samples(std::slice::from_ref(&samples(&key, &[100, 200]))).expect("written");
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
    a.add_samples(std::slice::from_ref(&samples(&key, &[100, 200]))).expect("written");
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
    let error = a.add_samples(std::slice::from_ref(&samples(&key, &[100]))).expect_err("down").1;
    assert!(
        matches!(error, StateError::Store(StoreError::Failed(_))),
        "{error:?}"
    );
    store.down.store(false, Ordering::SeqCst);
    store.fence.store(true, Ordering::SeqCst);
    let error = a.add_samples(std::slice::from_ref(&samples(&key, &[100]))).expect_err("fenced").1;
    assert!(a.superseded_by(&error), "{error:?}");
    assert!(a.estimates().expect("read").is_empty());
}

/// A batch of `count` keys, each of one level-2 row and one configured row.
fn batch(count: u64) -> Vec<Samples> {
    (0..count)
        .map(|n| {
            let key = Key::new(
                Instance::new("main"),
                PlatformClass::new("linux/x86_64"),
                Basis::Cgroup,
                Shape(n),
                Some(Cfg(n)),
            );
            samples(&key, &[100 + n])
        })
        .collect()
}

/// Behaviour: a batch of many keys is read with one scan and written in one transaction,
/// and every key's rows hold its runs. Catches: a store round trip per key.
#[test]
fn a_batch_is_one_read_and_one_transaction() {
    let store = Racing::new();
    let a = frontend(&store);
    let batch = batch(20);
    let reads = store.reads.load(Ordering::SeqCst);
    a.add_samples(&batch).expect("written");
    assert_eq!(store.reads.load(Ordering::SeqCst) - reads, 1);
    assert_eq!(store.est_attempts.load(Ordering::SeqCst), 1);
    for (key, _) in &batch {
        assert_eq!(stored(&a, key).expect("a summary").samples(), 1);
        assert_eq!(stored(&a, &key.l2()).expect("a summary").samples(), 1);
    }
}

/// Behaviour: a batch that lost the race to another frontend on one of its rows is applied
/// key by key on what the winner wrote: the rival's sample and every key's runs stay.
/// Catches: a batch conflict that drops the batch, or applies a run twice.
#[test]
fn a_batch_that_conflicts_is_applied_key_by_key() {
    let store = Racing::new();
    let a = frontend(&store);
    let batch = batch(4);
    let rival = samples(&batch[2].0, &[900]);
    store
        .rivals
        .lock()
        .expect("lock")
        .push(rival_write(&rival, &batch[2].0));
    a.add_samples(&batch).expect("written");
    // The batch, then each of the four keys.
    assert_eq!(store.est_attempts.load(Ordering::SeqCst), 5);
    assert_eq!(stored(&a, &batch[2].0).expect("a summary").samples(), 2);
    for (key, _) in [&batch[0], &batch[1], &batch[3]] {
        assert_eq!(stored(&a, key).expect("a summary").samples(), 1);
    }
}

/// Behaviour: when the store fails part way through the keys, the write stops there and
/// reports the first key not applied, so the caller keeps that key and the ones after it
/// and no key is applied twice. Catches: one failed attempt per remaining key.
#[test]
fn a_failure_stops_the_batch_at_the_first_key_not_applied() {
    let store = Racing::new();
    let a = frontend(&store);
    let batch = batch(4);
    // The batch's transaction conflicts; the first key's goes through; the second fails.
    let rival = samples(&batch[0].0, &[900]);
    store
        .rivals
        .lock()
        .expect("lock")
        .push(rival_write(&rival, &batch[0].0));
    store.fail_after.store(2, Ordering::SeqCst);
    let (from, error) = a.add_samples(&batch).expect_err("fails");
    assert_eq!(from, 1, "{error:?}");
    assert!(matches!(error, StateError::Store(_)), "{error:?}");
    assert_eq!(store.est_attempts.load(Ordering::SeqCst), 3);
    assert_eq!(stored(&a, &batch[0].0).expect("a summary").samples(), 2);
    assert!(stored(&a, &batch[1].0).is_none());
}

/// Behaviour: a batch of many keys over a store that cannot be read fails with the first
/// key not applied being the first, after one read. Catches: a read per key of a down store.
#[test]
fn a_batch_over_a_store_that_cannot_be_read_fails_after_one_read() {
    let store = Racing::new();
    let a = frontend(&store);
    store.down.store(true, Ordering::SeqCst);
    let reads = store.reads.load(Ordering::SeqCst);
    let (from, _) = a.add_samples(&batch(20)).expect_err("down");
    assert_eq!(from, 0);
    assert_eq!(store.reads.load(Ordering::SeqCst) - reads, 1);
}

/// Behaviour: a key that loses the race again and again is given up on after a bounded
/// number of reads, with a conflict error, and its runs are not applied. Catches: a retry
/// loop with no end.
#[test]
fn a_key_that_keeps_losing_the_race_is_given_up_on() {
    let store = Racing::new();
    let a = frontend(&store);
    let key = key(None);
    for n in 0..CONFLICT_RETRIES {
        let rival = samples(&key, &[500 + n as u64]);
        store
            .rivals
            .lock()
            .expect("lock")
            .push(rival_write(&rival, &key));
    }
    let (from, error) = a
        .add_samples(&[samples(&key, &[100])])
        .expect_err("gives up");
    assert_eq!(from, 0);
    assert!(matches!(error, StateError::Conflict(_)), "{error:?}");
    assert_eq!(
        store.est_attempts.load(Ordering::SeqCst) as usize,
        CONFLICT_RETRIES
    );
}
