//! The in-memory backend: the executable spec of the contract and the backend the
//! scheduler's simulations run against.
//!
//! A [`Fake`] keeps a live copy of its data and a durable copy. Every sync (it costs
//! [`FAKE_SYNC_COST`] of the injected clock) copies live to durable; [`Fake::restart`] is a
//! crash: it drops the live copy and reopens from the durable one. Group commit is
//! visible in the clock: a batch costs one sync however many transactions it holds.
//!
//! [`Flaw`] names one deliberate defect per clause. A fake built with a flaw must fail
//! the clause it breaks, which is what proves the clause can fail.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::clock::{Clock, ManualClock, StoreTime};
use crate::conformance::{Backend, C1_RACERS, RACE_KEY};
use crate::store::Store;
use crate::types::{
    Change, Changes, Commit, CommitStats, Cond, Conflict, Event, Op, Revision, StoreError, Txn,
    Version, Versioned,
};

/// How much store time one durable sync takes on the fake.
pub const FAKE_SYNC_COST: Duration = Duration::from_millis(2);

/// A deliberate defect, one per clause the suite checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flaw {
    /// A correct store.
    None,
    /// C1: compare-and-set checks its version, lets go of the lock, and writes later, so
    /// concurrent callers on one version all win. Only on [`RACE_KEY`].
    NonLinearizableCas,
    /// C2: preconditions are checked one operation at a time, as each applies, so a
    /// failure leaves the earlier operations written.
    PartialTxn,
    /// C3: counters are not part of what a sync makes durable, so a restart reissues values.
    CounterNotDurable,
    /// C4: expiry is judged by the clock reading taken when the store opened, standing in
    /// for a caller's clock, not the store's current one.
    CallerClockExpiry,
    /// C5: the feed returns rows past a revision that has not committed yet.
    FeedPastPending,
    /// C6: a write is acknowledged without the sync, so a crash loses it.
    AckBeforeSync,
    /// C7: every transaction pays its own sync, even inside a batch.
    Ungrouped,
}

#[derive(Clone)]
struct Row {
    value: Vec<u8>,
    version: Version,
    expires_at: Option<StoreTime>,
}

impl Row {
    fn alive(&self, now: StoreTime) -> bool {
        self.expires_at.is_none_or(|at| at > now)
    }

    fn versioned(&self) -> Versioned {
        Versioned {
            value: self.value.clone(),
            version: self.version,
            expires_at: self.expires_at,
        }
    }
}

#[derive(Clone, Default)]
struct Data {
    rows: BTreeMap<String, Row>,
    counters: BTreeMap<String, u64>,
    feed: BTreeMap<u64, Change>,
    /// The highest revision handed out.
    head: u64,
}

impl Data {
    /// Reserves `n` consecutive revisions and returns the first.
    fn alloc(&mut self, n: u64) -> u64 {
        let base = self.head + 1;
        self.head += n;
        base
    }

    fn live_row(&self, key: &str, now: StoreTime) -> Option<&Row> {
        self.rows.get(key).filter(|row| row.alive(now))
    }

    fn check(&self, key: &str, cond: Cond, index: usize, now: StoreTime) -> Result<(), Conflict> {
        let row = self.live_row(key, now);
        let met = match cond {
            Cond::Any => true,
            Cond::Absent => row.is_none(),
            Cond::Version(version) => row.is_some_and(|row| row.version == version),
        };
        if met {
            Ok(())
        } else {
            Err(Conflict {
                index,
                key: key.to_owned(),
            })
        }
    }

    fn check_all(&self, txn: &Txn, now: StoreTime) -> Result<(), Conflict> {
        for (index, op) in txn.ops().iter().enumerate() {
            if let Some((key, cond)) = op.key_cond() {
                self.check(key, cond, index, now)?;
            }
        }
        Ok(())
    }

    fn emit(&mut self, revision: u64, key: &str, event: Event) {
        let change = Change {
            revision: Revision(revision),
            key: key.to_owned(),
            event,
        };
        self.feed.insert(revision, change);
    }

    /// Removes the keys the clock has expired, one `Expire` row each; returns how many.
    fn reap(&mut self, now: StoreTime) -> usize {
        let due: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, row)| !row.alive(now))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &due {
            let revision = self.alloc(1);
            self.rows.remove(key);
            self.emit(revision, key, Event::Expire);
        }
        due.len()
    }

    /// Applies `txn` with the revisions from `base` on. With `check_each` a precondition
    /// is judged as its operation applies, not before the first one (the C2 flaw).
    fn write(
        &mut self,
        txn: &Txn,
        now: StoreTime,
        base: u64,
        check_each: bool,
    ) -> Result<Commit, Conflict> {
        let mut next_revision = base;
        let mut wrote = None;
        let mut counters = Vec::new();
        for (index, op) in txn.ops().iter().enumerate() {
            if check_each && let Some((key, cond)) = op.key_cond() {
                self.check(key, cond, index, now)?;
            }
            match op {
                Op::Put {
                    key, value, ttl, ..
                } => {
                    let revision = next_revision;
                    next_revision += 1;
                    let expires_at = ttl.map(|ttl| now.saturating_add(ttl));
                    let row = Row {
                        value: value.clone(),
                        version: Version(revision),
                        expires_at,
                    };
                    self.rows.insert(key.clone(), row);
                    self.emit(revision, key, Event::Put(value.clone()));
                    wrote = Some(Revision(revision));
                }
                Op::Delete { key, .. } => {
                    let revision = next_revision;
                    next_revision += 1;
                    if self.rows.remove(key).is_some() {
                        self.emit(revision, key, Event::Delete);
                        wrote = Some(Revision(revision));
                    }
                }
                Op::Check { .. } => {}
                Op::Next { counter } => {
                    let value = self.counters.entry(counter.clone()).or_insert(0);
                    *value += 1;
                    counters.push(*value);
                }
            }
        }
        Ok(Commit {
            revision: wrote.unwrap_or(Revision(self.head)),
            counters,
        })
    }
}

struct Inner {
    live: Data,
    durable: Data,
    /// The first revision of each transaction that has taken revisions and not yet
    /// committed (see [`Fake::prepare`]).
    pending: BTreeSet<u64>,
    stats: CommitStats,
}

impl Inner {
    fn reopened(durable: Data) -> Inner {
        Inner {
            live: durable.clone(),
            durable,
            pending: BTreeSet::new(),
            stats: CommitStats::default(),
        }
    }
}

/// Holds the callers of a racing compare-and-set until all have checked (the C1 flaw).
#[derive(Default)]
struct Gate {
    arrived: Mutex<usize>,
    all_here: Condvar,
}

impl Gate {
    fn arrive(&self, racers: usize) {
        let mut arrived = lock(&self.arrived);
        *arrived += 1;
        self.all_here.notify_all();
        let _released = self
            .all_here
            .wait_while(arrived, |arrived| *arrived < racers)
            .unwrap_or_else(PoisonError::into_inner);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn races_on(txn: &Txn, race_key: &str) -> bool {
    txn.ops()
        .iter()
        .any(|op| matches!(op.key_cond(), Some((key, Cond::Version(_))) if key == race_key))
}

/// A transaction that has taken its revisions but not committed (see [`Fake::prepare`]).
#[derive(Debug)]
pub struct Deferred {
    txn: Txn,
    base: u64,
}

/// The in-memory state store.
pub struct Fake {
    inner: Arc<Mutex<Inner>>,
    clock: Arc<ManualClock>,
    flaw: Flaw,
    /// The clock reading at open, for [`Flaw::CallerClockExpiry`].
    frozen: StoreTime,
    gate: Arc<Gate>,
}

impl Fake {
    /// An empty, correct store on `clock`.
    #[must_use]
    pub fn new(clock: Arc<ManualClock>) -> Fake {
        Fake::with_flaw(clock, Flaw::None)
    }

    /// An empty store on `clock` with `flaw`.
    #[must_use]
    pub fn with_flaw(clock: Arc<ManualClock>, flaw: Flaw) -> Fake {
        let frozen = clock.now();
        Fake {
            inner: Arc::new(Mutex::new(Inner::reopened(Data::default()))),
            clock,
            flaw,
            frozen,
            gate: Arc::new(Gate::default()),
        }
    }

    /// A crash and reopen: what was synced survives, the rest is gone.
    #[must_use]
    pub fn restart(self) -> Fake {
        let durable = lock(&self.inner).durable.clone();
        Fake {
            inner: Arc::new(Mutex::new(Inner::reopened(durable))),
            ..self
        }
    }

    /// Takes the revisions `txn` will write without committing it, as a second writer
    /// does in a store with concurrent transactions. The feed holds back every row from
    /// these revisions on until [`Fake::finish`] commits it.
    #[must_use]
    pub fn prepare(&self, txn: Txn) -> Deferred {
        let mut inner = lock(&self.inner);
        let base = inner.live.alloc(txn.change_count());
        inner.pending.insert(base);
        Deferred { txn, base }
    }

    /// Commits a prepared transaction.
    ///
    /// # Errors
    /// The [`Conflict`] of a precondition that no longer holds.
    pub fn finish(&self, deferred: Deferred) -> Result<Commit, Conflict> {
        let now = self.clock_now();
        let result = {
            let mut inner = lock(&self.inner);
            inner.live.reap(now);
            inner.pending.remove(&deferred.base);
            let checked = inner.live.check_all(&deferred.txn, now);
            let result =
                checked.and_then(|()| inner.live.write(&deferred.txn, now, deferred.base, false));
            if result.is_ok() {
                inner.stats.txns += 1;
            }
            result
        };
        if result.is_ok() {
            self.sync();
        }
        result
    }

    fn clock_now(&self) -> StoreTime {
        if self.flaw == Flaw::CallerClockExpiry {
            self.frozen
        } else {
            self.clock.now()
        }
    }

    /// Makes everything applied so far durable, for one sync's time.
    fn sync(&self) {
        if self.flaw == Flaw::AckBeforeSync {
            return;
        }
        let mut inner = lock(&self.inner);
        self.clock.advance(FAKE_SYNC_COST);
        inner.stats.syncs += 1;
        let mut snapshot = inner.live.clone();
        if self.flaw == Flaw::CounterNotDurable {
            snapshot.counters.clear();
        }
        inner.durable = snapshot;
    }

    fn apply_one(&self, txn: &Txn) -> Result<Commit, Conflict> {
        let now = self.clock_now();
        let racing = self.flaw == Flaw::NonLinearizableCas && races_on(txn, RACE_KEY);
        let mut inner = lock(&self.inner);
        inner.live.reap(now);
        if racing {
            // Check, let go, wait for the other racers, write without checking again.
            let verdict = inner.live.check_all(txn, now);
            drop(inner);
            self.gate.arrive(C1_RACERS);
            inner = lock(&self.inner);
            verdict?;
        } else if self.flaw != Flaw::PartialTxn {
            inner.live.check_all(txn, now)?;
        }
        let base = inner.live.alloc(txn.change_count());
        let commit = inner
            .live
            .write(txn, now, base, self.flaw == Flaw::PartialTxn)?;
        inner.stats.txns += 1;
        Ok(commit)
    }
}

impl Store for Fake {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let mut results = Vec::with_capacity(batch.len());
        for txn in &batch {
            let result = self.apply_one(txn);
            if self.flaw == Flaw::Ungrouped && result.is_ok() {
                self.sync();
            }
            results.push(result);
        }
        if self.flaw != Flaw::Ungrouped && results.iter().any(Result::is_ok) {
            self.sync();
        }
        Ok(results)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let now = self.clock_now();
        let inner = lock(&self.inner);
        Ok(inner.live.live_row(key, now).map(Row::versioned))
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let now = self.clock_now();
        let inner = lock(&self.inner);
        Ok(inner
            .live
            .rows
            .iter()
            .filter(|(key, row)| key.starts_with(prefix) && row.alive(now))
            .map(|(key, row)| (key.clone(), row.versioned()))
            .collect())
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        let inner = lock(&self.inner);
        // Rows from the first revision that can still commit on are not yet the poller's.
        let horizon = if self.flaw == Flaw::FeedPastPending {
            inner.live.head
        } else {
            inner
                .pending
                .first()
                .map_or(inner.live.head, |first| first - 1)
        };
        let rows: Vec<Change> = inner
            .live
            .feed
            .values()
            .filter(|change| change.revision.0 > after.0 && change.revision.0 <= horizon)
            .take(limit)
            .cloned()
            .collect();
        let cursor = if rows.len() >= limit {
            rows.last().map_or(after, |change| change.revision)
        } else {
            Revision(horizon.max(after.0))
        };
        Ok(Changes { rows, cursor })
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        let now = self.clock_now();
        let reaped = lock(&self.inner).live.reap(now);
        if reaped > 0 {
            self.sync();
        }
        Ok(reaped)
    }

    fn commit_stats(&self) -> CommitStats {
        lock(&self.inner).stats
    }

    fn now(&self) -> StoreTime {
        self.clock.now()
    }
}

/// The suite's factory for the fake, with the flaw to build it with.
#[derive(Clone, Copy, Debug)]
pub struct FakeBackend {
    flaw: Flaw,
}

impl FakeBackend {
    /// A factory for fakes with `flaw`.
    #[must_use]
    pub fn new(flaw: Flaw) -> FakeBackend {
        FakeBackend { flaw }
    }
}

impl Backend for FakeBackend {
    type Store = Fake;

    fn open(&self, clock: Arc<ManualClock>) -> Fake {
        Fake::with_flaw(clock, self.flaw)
    }

    fn restart(&self, store: Fake) -> Fake {
        store.restart()
    }

    fn commit_out_of_order(
        &self,
        store: &Fake,
        first: Txn,
        second: Txn,
        between: &mut dyn FnMut(),
    ) -> Result<(), Conflict> {
        let first = store.prepare(first);
        let second = store.prepare(second);
        store.finish(second)?;
        between();
        store.finish(first)?;
        Ok(())
    }
}
