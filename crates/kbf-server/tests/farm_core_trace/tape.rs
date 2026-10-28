//! The state store the farm writes through, with a tape of what it was asked to write.
//!
//! It is the in-memory fake on a clock the script moves by hand. Every transaction is
//! written down as the store saw it: the rows it put, deleted, checked and counted, and
//! whether it applied. A value is written as its length and the first bytes of its
//! SHA-256, so a changed byte in any row changes the tape; a key, a condition or a time to
//! live is written out.

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
            normalise,
        }
    }

    /// Moves the store's clock on, as the script moves tokio's.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The lines written since the last call, oldest first.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines())
    }

    /// A crash and reopen: what the store acknowledged survives. The tape goes on.
    pub fn restart(self) -> Self {
        Self {
            inner: self.inner.restart(),
            clock: self.clock,
            lines: self.lines,
            normalise: self.normalise,
        }
    }

    fn lines(&self) -> MutexGuard<'_, Vec<String>> {
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}
