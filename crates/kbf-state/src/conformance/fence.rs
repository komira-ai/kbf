//! The fencing clauses, generic over a [`Backend`] so the fake, SQLite and Postgres run
//! the same cases: the generation row, the handle that checks it on every write and on
//! every reap, and the race the check exists to close.
//!
//! Every case names the defect it catches. None reads the wall clock.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::Backend;
use crate::clock::{ManualClock, StoreTime};
use crate::store::Store;
use crate::types::{
    Changes, Commit, CommitStats, Cond, Conflict, Revision, ScanPage, ScanRange, StoreError, Txn,
    Versioned,
};
use crate::{Fenced, GENERATION, TakeError};

fn open<B: Backend>(backend: &B) -> (Arc<dyn Store>, Arc<ManualClock>)
where
    B::Store: 'static,
{
    let clock = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> = Arc::new(backend.open(Arc::clone(&clock)));
    (store, clock)
}

/// A store that runs a hook once, just before the first write or reap reaches the store
/// underneath. It stands for the moment between a handle's check and its write: the hook
/// is where a rival takes the next generation.
struct Interpose {
    inner: Arc<dyn Store>,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Interpose {
    fn new(inner: Arc<dyn Store>) -> Interpose {
        Interpose {
            inner,
            hook: Mutex::new(None),
        }
    }

    fn arm(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }

    fn fire(&self) {
        let hook = self
            .hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

impl Store for Interpose {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.fire();
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
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

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.fire();
        self.inner.reap_expired()
    }

    fn reap_expired_if(
        &self,
        key: &str,
        cond: Cond,
    ) -> Result<Result<usize, Conflict>, StoreError> {
        self.fire();
        self.inner.reap_expired_if(key, cond)
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// F1: the first writer of an empty store holds 1, the next holds 2. Catches a generation
/// that does not count up from the store's own row.
pub fn f1_each_taker_holds_the_next_generation<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    let first = Fenced::take(Arc::clone(&store), None).expect("take");
    assert_eq!(first.generation(), 1, "F1 first generation");
    let second = Fenced::take(Arc::clone(&store), Some(1)).expect("take");
    assert_eq!(second.generation(), 2, "F1 second generation");
}

/// F2: a taker that expects another generation is refused and writes nothing; a row that
/// is not a number is refused. Catches a successor that takes over a file that is not the
/// one it was told about (a restored backup, a swapped file).
pub fn f2_a_taker_that_expects_another_generation_is_refused<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    Fenced::take(Arc::clone(&store), None).expect("take");
    let before = store.get(GENERATION).expect("get").expect("row");
    let refused = Fenced::take(Arc::clone(&store), Some(7)).expect_err("another generation");
    assert_eq!(
        refused,
        TakeError::Mismatch {
            expected: 7,
            found: 1
        },
        "F2 mismatch"
    );
    assert_eq!(
        store.get(GENERATION).expect("get"),
        Some(before),
        "F2 a refused take wrote the row"
    );
    let (corrupt, _) = open(backend);
    corrupt
        .txn(Txn::new().put(GENERATION, "x", Cond::Any))
        .expect("store")
        .expect("put");
    assert_eq!(
        Fenced::take(corrupt, None).expect_err("not a number"),
        TakeError::Corrupt,
        "F2 corrupt row"
    );
}

/// F3: when many handles take the generation of one store at once, no two hold the same
/// generation. Catches a lost update in the take.
pub fn f3_concurrent_takers_never_hold_the_same_generation<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    let takers: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || Fenced::take(store, None))
        })
        .collect();
    let mut held: Vec<u64> = takers
        .into_iter()
        .filter_map(|t| t.join().expect("join").ok())
        .map(|f| f.generation())
        .collect();
    held.sort_unstable();
    let distinct = held.len();
    held.dedup();
    assert_eq!(held.len(), distinct, "F3 two handles hold one generation");
    assert!(!held.is_empty(), "F3 at least one taker won");
}

/// F4: once another handle takes the next generation, a put, a delete, a time-to-live
/// put, a counter and a check-only transaction of the old handle each fail `Fenced` and
/// change nothing, and so do a batch and a reap. Catches a write path that bypasses the
/// fence.
pub fn f4_a_superseded_handle_fails_every_kind_of_write<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    let old = Fenced::take(Arc::clone(&store), None).expect("take");
    old.txn(Txn::new().put("k", "v", Cond::Any))
        .expect("the holder writes")
        .expect("commits");
    let new = Fenced::take(Arc::clone(&store), Some(1)).expect("take");
    let writes = [
        Txn::new().put("k", "stale", Cond::Any),
        Txn::new().delete("k", Cond::Any),
        Txn::new().put_ttl("t", "v", Duration::from_secs(5), Cond::Any),
        Txn::new().next("counter"),
        Txn::new().check("k", Cond::Any),
        Txn::new(),
    ];
    for txn in writes {
        assert_eq!(
            old.txn(txn.clone()).unwrap_err(),
            StoreError::Fenced,
            "F4 a superseded write landed"
        );
        new.txn(txn)
            .expect("the new holder writes")
            .expect("commits");
    }
    assert_eq!(
        old.apply_batch(vec![Txn::new().put("a", "1", Cond::Any)])
            .unwrap_err(),
        StoreError::Fenced,
        "F4 a superseded batch landed"
    );
    assert_eq!(
        old.reap_expired().unwrap_err(),
        StoreError::Fenced,
        "F4 a superseded reap ran"
    );
    assert_eq!(
        old.get("a").expect("reads are not fenced"),
        None,
        "F4 the fenced batch wrote"
    );
    assert_eq!(new.reap_expired(), Ok(0), "F4 the holder reaps");
}

/// F5: a lost compare-and-set of the live holder is a conflict, not `Fenced`. Catches a
/// precondition failure mistaken for a fence.
pub fn f5_the_holder_still_sees_its_own_conflicts<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    let held = Fenced::take(store, None).expect("take");
    held.txn(Txn::new().put("k", "v", Cond::Absent))
        .expect("store")
        .expect("commits");
    let conflict = held
        .txn(Txn::new().put("k", "w", Cond::Absent))
        .expect("the store answers")
        .expect_err("the key exists");
    assert_eq!(conflict.key, "k", "F5 conflict key");
}

/// F6: reads, the feed, the clock and the counters all come from the store underneath.
/// Catches a handle that forwards the wrong things.
pub fn f6_reads_pass_through<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    // Through a plain forwarding layer too, so a layer that drops a read is caught here.
    let layer: Arc<dyn Store> = Arc::new(Interpose::new(Arc::clone(&store)));
    let held = Fenced::take(layer, None).expect("take");
    held.txn(
        Txn::new()
            .put("p/a", "1", Cond::Any)
            .put("p/b", "2", Cond::Any),
    )
    .expect("store")
    .expect("commit");
    assert_eq!(held.scan_prefix("p/").expect("scan").len(), 2, "F6 scan");
    let page = held
        .scan_range(ScanRange::new("p/", std::num::NonZeroUsize::MIN))
        .expect("range");
    assert_eq!(page.rows.len(), 1, "F6 range");
    assert!(
        held.changes(Revision::ZERO, 100).expect("feed").rows.len() >= 3,
        "F6 feed"
    );
    assert_eq!(held.now(), store.now(), "F6 clock");
    assert_eq!(held.commit_stats(), store.commit_stats(), "F6 stats");
}

/// F7: a handle superseded after its check but before its write lands is fenced and
/// writes nothing, while the rival writes. Catches a fence checked outside the write's
/// own transaction (read the generation, then write): the rival's take lands in the gap.
pub fn f7_a_write_superseded_before_it_lands_is_fenced<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, _) = open(backend);
    let between = Arc::new(Interpose::new(Arc::clone(&store)));
    let old = Fenced::take(Arc::clone(&between) as Arc<dyn Store>, None).expect("take");
    let rival: Arc<Mutex<Option<Fenced>>> = Arc::default();
    let seat = Arc::clone(&rival);
    let raw = Arc::clone(&store);
    between.arm(move || {
        let taken = Fenced::take(raw, Some(1)).expect("the rival takes the next generation");
        *seat.lock().unwrap_or_else(PoisonError::into_inner) = Some(taken);
    });
    assert_eq!(
        old.txn(Txn::new().put("k", "stale", Cond::Any))
            .unwrap_err(),
        StoreError::Fenced,
        "F7 a write superseded before it landed"
    );
    assert_eq!(store.get("k").expect("read"), None, "F7 the stale write");
    let rival = rival
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("the hook ran");
    assert_eq!(rival.generation(), 2, "F7 rival generation");
    rival
        .txn(Txn::new().put("k", "live", Cond::Any))
        .expect("store")
        .expect("the rival writes");
}

/// F8: a handle superseded after its check but before its reap runs is fenced and reaps
/// nothing. Catches a reap that checks the generation in a read of its own and then
/// reaps (read, then write: the rival's take lands in the gap).
pub fn f8_a_reap_superseded_before_it_runs_is_fenced<B: Backend>(backend: &B)
where
    B::Store: 'static,
{
    let (store, clock) = open(backend);
    let between = Arc::new(Interpose::new(Arc::clone(&store)));
    let old = Fenced::take(Arc::clone(&between) as Arc<dyn Store>, None).expect("take");
    old.txn(Txn::new().put_ttl("t", "v", Duration::from_secs(5), Cond::Any))
        .expect("store")
        .expect("commit");
    clock.advance(Duration::from_secs(10));
    let rival: Arc<Mutex<Option<Fenced>>> = Arc::default();
    let seat = Arc::clone(&rival);
    let raw = Arc::clone(&store);
    between.arm(move || {
        let taken = Fenced::take(raw, Some(1)).expect("the rival takes the next generation");
        *seat.lock().unwrap_or_else(PoisonError::into_inner) = Some(taken);
    });
    assert_eq!(
        old.reap_expired(),
        Err(StoreError::Fenced),
        "F8 a reap superseded before it ran"
    );
    assert!(
        rival
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some(),
        "F8 the hook ran"
    );
    // The unguarded reap forwards (how many keys it finds depends on whether the backend
    // reaps on every write, so only that it runs is asserted).
    assert!(
        between.reap_expired().is_ok(),
        "F8 an unguarded reap forwards"
    );
}
