//! The holder's liveness announcement on the backend-neutral side: the default for a
//! backend that cannot announce, the guard, and `Fenced::take_holding` ordering (the
//! announcement is made before the generation row is written, and a take that fails
//! leaves neither behind). The Postgres mechanism is postgres_liveness.rs.
//!
//! Every test names the defect it catches.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Fenced, GENERATION, LivenessGuard, ManualClock, Op,
    Revision, ScanPage, ScanRange, Store, StoreError, StoreTime, TakeError, Txn, Versioned,
};

fn fake() -> Fake {
    Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None)
}

/// The fake, announcing liveness in a shared set, so a test sees who is "alive".
struct Lively {
    fake: Fake,
    alive: Arc<Mutex<BTreeSet<u64>>>,
}

struct Announcement {
    alive: Arc<Mutex<BTreeSet<u64>>>,
    generation: u64,
}

impl kbf_state::Announcement for Announcement {
    fn is_held(&self) -> bool {
        self.alive
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&self.generation)
    }
}

impl Drop for Announcement {
    fn drop(&mut self) {
        self.alive
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.generation);
    }
}

impl Lively {
    fn new() -> Lively {
        Lively {
            fake: fake(),
            alive: Arc::default(),
        }
    }

    fn alive_set(&self) -> BTreeSet<u64> {
        self.alive
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Store for Lively {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        // The announcement must come first: a write of the generation row finds that
        // generation announced already.
        for op in batch.iter().flat_map(|txn| txn.ops()) {
            if let Op::Put { key, value, .. } = op
                && key == GENERATION
            {
                let written = u64::from_be_bytes(value.as_slice().try_into().expect("8 bytes"));
                assert!(
                    self.alive_set().contains(&written),
                    "generation {written} was written before it was announced"
                );
            }
        }
        self.fake.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.fake.get(key)
    }
    fn scan_prefix(&self, p: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.fake.scan_prefix(p)
    }
    fn scan_range(&self, s: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.fake.scan_range(s)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.fake.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.fake.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.fake.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.fake.now()
    }
    fn hold_liveness(&self, generation: u64) -> Result<LivenessGuard, StoreError> {
        let mut alive = self.alive.lock().unwrap_or_else(PoisonError::into_inner);
        if !alive.insert(generation) {
            return Err(StoreError::Failed("already held".to_owned()));
        }
        Ok(LivenessGuard::new(Announcement {
            alive: Arc::clone(&self.alive),
            generation,
        }))
    }
    fn holder_alive(&self, generation: u64) -> Result<bool, StoreError> {
        Ok(self.alive_set().contains(&generation))
    }
}

// Catches a backend that cannot announce pretending it can: the default says so, and
// `take_holding` on it writes nothing (no generation row, no half-taken role).
#[test]
fn a_backend_that_cannot_announce_says_so_and_take_holding_writes_nothing() {
    let store = Arc::new(fake());
    assert!(matches!(
        store.hold_liveness(1),
        Err(StoreError::Failed(why)) if why.contains("cannot announce")
    ));
    assert!(matches!(
        store.holder_alive(1),
        Err(StoreError::Failed(why)) if why.contains("cannot tell")
    ));
    let refused = Fenced::take_holding(store.clone(), None).expect_err("cannot announce");
    assert!(matches!(refused, TakeError::Store(StoreError::Failed(_))));
    assert_eq!(store.get(GENERATION).expect("get"), None);
}

// Catches a guard that does not own what announces: dropping it must drop the thing it
// holds (for Postgres, the connection that holds the lock).
#[test]
fn dropping_the_guard_drops_what_it_holds() {
    struct Flag(Arc<AtomicBool>);
    impl kbf_state::Announcement for Flag {
        fn is_held(&self) -> bool {
            !self.0.load(Ordering::SeqCst)
        }
    }
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = LivenessGuard::new(Flag(Arc::clone(&dropped)));
    assert_eq!(format!("{guard:?}"), "LivenessGuard");
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(guard.is_held(), "the guard asks what it owns");
    drop(guard);
    assert!(dropped.load(Ordering::SeqCst));
}

// Catches a take that announces after it writes (a window where the generation is held
// and its holder reads as dead), and a handle whose guard does not stay up: the
// announcement exists once the take returns, for the generation taken, and goes with the
// guard. The store above asserts the order on every generation write.
#[test]
fn take_holding_announces_the_generation_before_writing_it() {
    let store = Arc::new(Lively::new());
    let (first, guard) = Fenced::take_holding(store.clone(), None).expect("take");
    assert_eq!(first.generation(), 1);
    assert_eq!(first.holder_alive(1), Ok(true));
    let (second, second_guard) = Fenced::take_holding(store.clone(), Some(1)).expect("take");
    assert_eq!(second.generation(), 2);
    assert_eq!(store.alive_set(), BTreeSet::from([1, 2]));
    drop(guard);
    assert_eq!(
        second.holder_alive(1),
        Ok(false),
        "the first holder is gone"
    );
    assert_eq!(second.holder_alive(2), Ok(true));
    drop(second_guard);
    assert!(store.alive_set().is_empty());
}

// Catches a failed take that leaves its announcement up: an expectation that is wrong
// is refused before anything is announced, and a generation that is already announced
// (a rival is taking it) is refused with the row unwritten.
#[test]
fn a_take_that_fails_announces_nothing_and_writes_nothing() {
    let store = Arc::new(Lively::new());
    let (_held, _guard) = Fenced::take_holding(store.clone(), None).expect("take");
    let mismatch = Fenced::take_holding(store.clone(), Some(5)).expect_err("wrong generation");
    assert_eq!(
        mismatch,
        TakeError::Mismatch {
            expected: 5,
            found: 1
        }
    );
    assert_eq!(store.alive_set(), BTreeSet::from([1]));
    let rival = store
        .hold_liveness(2)
        .expect("a rival announces generation 2");
    let row = store.get(GENERATION).expect("get");
    let refused = Fenced::take_holding(store.clone(), Some(1)).expect_err("already announced");
    assert!(matches!(refused, TakeError::Store(StoreError::Failed(_))));
    assert_eq!(store.get(GENERATION).expect("get"), row, "the row moved");
    drop(rival);
}
