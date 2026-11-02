//! The state store the farm writes through, with a tape of what it was asked to write.
//!
//! It is the in-memory fake on a clock the script moves by hand. Every transaction is
//! written down as the store saw it: the rows it put, deleted, checked and counted, and
//! whether it applied. A value is written as its length and the first bytes of its
//! SHA-256, so a changed byte in any row changes the tape; a key, a condition or a time to
//! live is written out.
//!
//! A script can make rows unreadable ([`Tape::unreadable`]): a read that would return one fails
//! as a backend failure does, and the rows stay as they are. A read that succeeds is not written
//! down; one refused is, as `read refused <key>`, once per row in the lines a call takes. The
//! rows stay unreadable across a restart of the store, until [`Tape::readable`]; the change feed
//! and the expiry of rows do not look at them.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use sha2::{Digest as _, Sha256};

/// A [`Fake`] store and the lines it has been asked to write since they were last taken.
pub struct Tape {
    inner: Fake,
    clock: Arc<ManualClock>,
    lines: Mutex<Vec<String>>,
    /// The keys whose rows cannot be read.
    unreadable: Mutex<BTreeSet<String>>,
    /// The unreadable keys a read was refused at since the lines were last taken.
    refused: Mutex<BTreeSet<String>>,
    /// How a value is written down, given its key.
    normalise: Normalise,
}

/// How the tape writes down the value put at a key: the bytes whose length and hash it
/// records.
pub type Normalise = fn(&str, &[u8]) -> Vec<u8>;

/// The value as the store was given it.
pub fn as_written(_key: &str, value: &[u8]) -> Vec<u8> {
    value.to_vec()
}

impl Tape {
    /// An empty store whose clock reads zero, writing values down as `normalise` says.
    pub fn new(normalise: Normalise) -> Self {
        let clock = Arc::new(ManualClock::default());
        Self {
            inner: Fake::with_flaw(Arc::clone(&clock), Flaw::None),
            clock,
            lines: Mutex::new(Vec::new()),
            unreadable: Mutex::new(BTreeSet::new()),
            refused: Mutex::new(BTreeSet::new()),
            normalise,
        }
    }

    /// Moves the store's clock on, as the script moves tokio's.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The lines written since the last call, oldest first.
    pub fn take(&self) -> Vec<String> {
        lock(&self.refused).clear();
        std::mem::take(&mut *self.lines())
    }

    /// A crash and reopen: what the store acknowledged survives. The tape goes on.
    pub fn restart(self) -> Self {
        Self {
            inner: self.inner.restart(),
            clock: self.clock,
            lines: self.lines,
            unreadable: self.unreadable,
            refused: self.refused,
            normalise: self.normalise,
        }
    }

    /// From now on, a read that would return the row at `key` fails: a `get` of it, and a
    /// scan whose answer holds it. Writes of it go on as before.
    pub fn unreadable(&self, key: &str) {
        self.unreadable_keys().insert(key.to_owned());
    }

    /// Every row can be read again.
    pub fn readable(&self) {
        self.unreadable_keys().clear();
    }

    fn lines(&self) -> MutexGuard<'_, Vec<String>> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn unreadable_keys(&self) -> MutexGuard<'_, BTreeSet<String>> {
        lock(&self.unreadable)
    }

    /// The failure of a read that met the row at `key`, if it cannot be read; the first refusal
    /// at `key` since the lines were last taken is written down.
    fn check(&self, key: &str) -> Result<(), StoreError> {
        if !self.unreadable_keys().contains(key) {
            return Ok(());
        }
        if lock(&self.refused).insert(key.to_owned()) {
            self.lines().push(format!("  read refused {key}"));
        }
        Err(StoreError::Failed(format!("the row {key} cannot be read")))
    }

    /// Fails if the row at any of `keys` cannot be read.
    fn check_all<'a>(&self, mut keys: impl Iterator<Item = &'a str>) -> Result<(), StoreError> {
        keys.try_for_each(|key| self.check(key))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cond(cond: Cond) -> String {
    match cond {
        Cond::Any => "any".to_owned(),
        Cond::Absent => "absent".to_owned(),
        Cond::Version(version) => format!("v{}", version.0),
    }
}

/// The length of `value` and the first eight bytes of its SHA-256, in hex.
fn bytes(value: &[u8]) -> String {
    let hash = Sha256::digest(value);
    format!("len={} sha={}", value.len(), hex::encode(&hash[..8]))
}

fn op(op: &Op, normalise: Normalise) -> String {
    match op {
        Op::Put {
            key,
            value,
            ttl,
            cond: c,
        } => {
            let ttl = ttl.map_or_else(|| "-".to_owned(), |t| format!("{}ms", t.as_millis()));
            let value = bytes(&normalise(key, value));
            format!("    put {key} cond={} {value} ttl={ttl}", cond(*c))
        }
        Op::Delete { key, cond: c } => format!("    del {key} cond={}", cond(*c)),
        Op::Check { key, cond: c } => format!("    check {key} cond={}", cond(*c)),
        Op::Next { counter } => format!("    next {counter}"),
    }
}

fn txn(txn: &Txn, applied: Option<&Result<Commit, Conflict>>, normalise: Normalise) -> Vec<String> {
    let head = match applied {
        Some(Ok(commit)) => {
            let counters: Vec<String> = commit.counters.iter().map(u64::to_string).collect();
            format!(
                "  txn applied rev={} counters=[{}]",
                commit.revision.0,
                counters.join(",")
            )
        }
        Some(Err(conflict)) => format!("  txn refused op={} key={}", conflict.index, conflict.key),
        None => "  txn failed".to_owned(),
    };
    std::iter::once(head)
        .chain(txn.ops().iter().map(|one| op(one, normalise)))
        .collect()
}

impl Store for Tape {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let result = self.inner.apply_batch(batch.clone());
        let mut lines = self.lines();
        for (index, one) in batch.iter().enumerate() {
            let applied = result.as_ref().ok().and_then(|all| all.get(index));
            lines.extend(txn(one, applied, self.normalise));
        }
        result
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let row = self.inner.get(key)?;
        if row.is_some() {
            self.check(key)?;
        }
        Ok(row)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let rows = self.inner.scan_prefix(prefix)?;
        self.check_all(rows.iter().map(|(key, _)| key.as_str()))?;
        Ok(rows)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        let page = self.inner.scan_range(scan)?;
        self.check_all(page.rows.iter().map(|(key, _)| key.as_str()))?;
        Ok(page)
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
