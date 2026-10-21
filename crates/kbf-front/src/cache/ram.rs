//! The RAM budget of the held set, its drain to the hot disk, the shutdown drain and the
//! metrics (`docs/design/storage-ram-ack.md`, rule 3 and "Drain and shutdown").
//!
//! **Nothing puts a blob in the set in a running node yet** (the acknowledgement point is a
//! later change): with `--ram-ack-bytes` at 0 every [`Cache::hold`] is refused, the set is
//! empty, a drain pass finds nothing and the gauges read 0.
//!
//! **The budget** ([`RamBudget`]) is per node and never waited for. [`Cache::hold`] makes a
//! non-blocking try-acquire of the blob's bytes; when the budget cannot give them the blob
//! is not held, the caller writes it through to its disk (and waits for it, which is the
//! backpressure), and `writethrough{reason=budget}` counts it. A permit returns to the
//! budget only when the blob leaves the set, that is when a local drain has put it in the
//! index, so no write ever waits on a peer for it.
//!
//! **The drain** moves each held blob to the hot disk through the write pool, which syncs
//! it and applies `PutStaged` (`pool_commit`, the path a blob takes today); a blob the tier
//! does not take goes to the object store. Only when the index has the blob does the entry
//! leave the set. A failed attempt leaves it in the set as `Failing{n}`, counted, logged,
//! and taken again after a capped backoff: an entry that is not durable is never dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::ObjectStore;
use kbf_types::Digest;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::pool::PoolCommit;
use super::{Cache, VerifiedBlob, is_empty_blob};
use crate::meta_log::MetaLog;
use crate::metrics::{Counter, Families};

/// The most blobs one drain pass takes: the rest wait for the next pass, which follows at
/// once.
pub const PASS_BLOBS: usize = 1024;

/// How long a shutdown drain waits before it takes the blobs a failed pass left, so a
/// disk that keeps failing is not asked in a tight loop.
const SHUTDOWN_RETRY: Duration = Duration::from_millis(100);

/// The bytes a node may hold in RAM as acknowledged and not yet on its disk
/// (`--ram-ack-bytes`). Shared by the permits it gives out.
#[derive(Debug, Default)]
pub(super) struct RamBudget {
    cap: u64,
    used: AtomicU64,
}

impl RamBudget {
    fn new(cap: u64) -> Arc<Self> {
        Arc::new(Self {
            cap,
            used: AtomicU64::new(0),
        })
    }

    /// `bytes` of the budget, or `None` if it has less than that left. Never waits.
    fn try_acquire(self: &Arc<Self>, bytes: u64) -> Option<Permit> {
        let taken = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.cap)
            });
        taken.ok().map(|_| Permit {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

/// Bytes of the budget, returned when it is dropped.
#[derive(Debug)]
pub(super) struct Permit {
    budget: Arc<RamBudget>,
    bytes: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Why a write was committed to disk before it was acknowledged, and not acknowledged from
/// RAM (`kbf_storage_ram_writethrough_total{reason}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteThrough {
    /// The replica could not be reached; a repair debt was journaled.
    ReplicaLost,
    /// The blob has one owner.
    SingleOwner,
    /// The blob is over the pool's buffered size.
    Large,
    /// The pool's queue was full.
    QueueFull,
    /// The RAM budget could not give the blob's bytes.
    Budget,
    /// The blob took the synchronous store path.
    StorePath,
}

impl WriteThrough {
    const ALL: [Self; 6] = [
        Self::ReplicaLost,
        Self::SingleOwner,
        Self::Large,
        Self::QueueFull,
        Self::Budget,
        Self::StorePath,
    ];

    /// The `reason` label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ReplicaLost => "replica_lost",
            Self::SingleOwner => "single_owner",
            Self::Large => "large",
            Self::QueueFull => "queue_full",
            Self::Budget => "budget",
            Self::StorePath => "store_path",
        }
    }
}

/// What [`Cache::hold`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The blob is held now, and its bytes are counted against the budget.
    Held,
    /// The blob needs no entry: it is held already (the first writer's entry stays and is
    /// the only one drained), or it is the empty blob, which is always present.
    Duplicate,
    /// The budget has not the bytes: the blob is not held, and the caller writes it
    /// through to its disk.
    Budget,
}

/// What a node counts of the RAM it holds, and the wake-up of its drain.
#[derive(Debug)]
pub(super) struct Ram {
    pub(super) budget: Arc<RamBudget>,
    drain_failures: Counter,
    writethrough: [Counter; WriteThrough::ALL.len()],
    /// Rung when a blob is held: the background drain waits on it.
    kick: Notify,
}

impl Default for Ram {
    fn default() -> Self {
        Self {
            budget: RamBudget::new(0),
            drain_failures: Counter::default(),
            writethrough: Default::default(),
            kick: Notify::new(),
        }
    }
}

impl Ram {
    /// Counts a write that was committed to disk for `reason`.
    pub(super) fn through(&self, reason: WriteThrough) {
        self.writethrough[reason as usize].add(1);
    }

    /// Appends the `kbf_storage_ram_*` families to `out`: the gauges as of `held`, the
    /// counters since the start.
    pub(super) fn render(&self, out: &mut String, held: super::held::Unwritten) {
        let mut f = Families(std::mem::take(out));
        f.gauge(
            "kbf_storage_ram_unwritten_bytes",
            "Bytes of blobs acknowledged from RAM and not yet in the index (a restart loses them)",
            held.bytes,
        );
        f.gauge(
            "kbf_storage_ram_unwritten_blobs",
            "Blobs acknowledged from RAM and not yet in the index",
            held.blobs,
        );
        f.head(
            "kbf_storage_ram_unwritten_oldest_seconds",
            "gauge",
            "Age of the oldest blob acknowledged from RAM and not yet in the index",
        );
        let _ = std::fmt::Write::write_fmt(
            &mut f.0,
            format_args!(
                "kbf_storage_ram_unwritten_oldest_seconds {}\n",
                held.oldest.as_secs_f64()
            ),
        );
        f.gauge(
            "kbf_storage_ram_budget_bytes",
            "Bytes of RAM the node may hold acknowledged and not yet on its disk (--ram-ack-bytes)",
            self.budget.cap,
        );
        f.counter(
            "kbf_storage_ram_drain_failures_total",
            "Attempts to write a blob acknowledged from RAM to the node's disk that failed (the blob stays held and is tried again)",
            self.drain_failures.get(),
        );
        let name = "kbf_storage_ram_writethrough_total";
        f.head(
            name,
            "counter",
            "Writes committed to disk before they were acknowledged, by the reason they were not acknowledged from RAM",
        );
        for reason in WriteThrough::ALL {
            let _ = std::fmt::Write::write_fmt(
                &mut f.0,
                format_args!(
                    "{name}{{reason=\"{}\"}} {}\n",
                    reason.label(),
                    self.writethrough[reason as usize].get()
                ),
            );
        }
        *out = f.0;
    }
}

/// What one drain pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Blobs the pass took.
    pub claimed: usize,
    /// Of them, blobs now in the index and gone from the set.
    pub written: usize,
    /// Of them, blobs that failed and stay held.
    pub failed: usize,
    /// When the earliest failed blob is due again, if any waits.
    pub retry_at: Option<Instant>,
}

/// How a shutdown drain ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownDrain {
    /// Nothing is held any more: every acknowledged blob is in the index.
    Drained,
    /// The deadline ended with these blobs and bytes still held. A restart loses them.
    TimedOut {
        /// Blobs still held.
        blobs: u64,
        /// Their bytes.
        bytes: u64,
    },
}

/// Gives back the blobs of a pass that ended without answering them (its future was
/// dropped), so another pass takes them.
struct Unclaim<'a, M, O> {
    cache: &'a Cache<M, O>,
    digests: Vec<Digest>,
}

impl<M, O> Drop for Unclaim<'_, M, O> {
    fn drop(&mut self) {
        let now = Instant::now();
        for digest in &self.digests {
            self.cache.held.unclaim(digest, now);
        }
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// The same cache, giving blobs held in RAM at most `bytes` of budget
    /// (`--ram-ack-bytes`). 0, the default, is off: [`Cache::hold`] refuses every blob. Set
    /// it before the cache holds anything.
    #[must_use]
    pub fn with_ram_ack_bytes(mut self, bytes: u64) -> Self {
        self.ram.budget = RamBudget::new(bytes);
        self
    }

    /// Holds `blob` as acknowledged and not yet on disk: its bytes are taken from the
    /// budget without waiting, the set keeps the blob for every reader, and the drain is
    /// woken. See [`Hold`] for the answers. A refusal for the budget is counted as
    /// `writethrough{reason=budget}`.
    ///
    /// Nothing in a running node calls this yet.
    pub fn hold(&self, blob: &VerifiedBlob) -> Hold {
        let digest = blob.digest();
        if is_empty_blob(&digest) || self.held.has(&digest) {
            return Hold::Duplicate;
        }
        let Some(permit) = self.ram.budget.try_acquire(blob.bytes.len() as u64) else {
            self.ram.through(WriteThrough::Budget);
            return Hold::Budget;
        };
        self.insert_held(blob, Some(permit))
    }

    /// Holds `blob` with no budget, as a test that is not about the budget does; whether
    /// it did (`false`: the digest was held already). Only for tests.
    #[cfg(feature = "testkit")]
    pub fn hold_for_test(&self, blob: &VerifiedBlob) -> bool {
        self.insert_held(blob, None) == Hold::Held
    }

    fn insert_held(&self, blob: &VerifiedBlob, permit: Option<Permit>) -> Hold {
        let inserted =
            self.held
                .try_insert(blob.digest(), blob.bytes.clone(), permit, Instant::now());
        if !inserted {
            // The permit went with the refused entry.
            return Hold::Duplicate;
        }
        self.ram.kick.notify_one();
        Hold::Held
    }

    /// What the set holds now: the `kbf_storage_ram_unwritten_*` gauges.
    #[must_use]
    pub fn unwritten(&self) -> super::held::Unwritten {
        self.held.unwritten(Instant::now())
    }

    /// Appends the RAM families to a scrape.
    pub(super) fn render_ram(&self, out: &mut String) {
        self.ram.render(out, self.held.unwritten(Instant::now()));
    }

    /// One drain pass at `now`: takes the held blobs that are queued, or failed and due,
    /// and writes them to the hot disk through the write pool (to the object store if the
    /// tier does not take them). A blob leaves the set once the index has it; a blob that
    /// failed stays, counted, due again after a capped backoff from `now`.
    pub async fn drain_held(&self, now: Instant) -> DrainReport {
        self.drain_pass(now, false).await
    }

    async fn drain_pass(&self, now: Instant, force: bool) -> DrainReport {
        let claimed = self.held.claim(now, force, PASS_BLOBS);
        let guard = Unclaim {
            cache: self,
            digests: claimed.iter().map(|(digest, _)| *digest).collect(),
        };
        let outcomes =
            futures::future::join_all(claimed.into_iter().map(|(digest, bytes)| async move {
                (digest, self.drain_one(digest, bytes).await)
            }))
            .await;
        let mut report = DrainReport {
            claimed: outcomes.len(),
            ..DrainReport::default()
        };
        for (digest, outcome) in outcomes {
            match outcome {
                Ok(()) => {
                    // The index has the blob: only now does the entry, and its budget, go.
                    self.held.remove(&digest);
                    report.written += 1;
                }
                Err(error) => {
                    report.failed += 1;
                    self.ram.drain_failures.add(1);
                    let failures = self.held.failed(&digest, now).map(|(n, _)| n);
                    tracing::warn!(
                        %error,
                        digest = %digest,
                        failures,
                        "a blob acknowledged from RAM could not be written to disk; it stays held and is tried again"
                    );
                }
            }
        }
        drop(guard);
        report.retry_at = self.held.next_due();
        report
    }

    /// Writes `bytes` as `digest` through the pool, or to the store if there is no pool.
    async fn drain_one(&self, digest: Digest, bytes: Bytes) -> Result<(), super::CacheError> {
        match self.write_pool() {
            Some(pool) => {
                let commit = PoolCommit::Whole {
                    bytes,
                    jobs: pool.jobs(),
                };
                self.pool_commit(commit, digest).await
            }
            None => self.store_whole(digest, bytes).await,
        }
    }

    /// Drains the set in the background for as long as the task lives: a pass when a blob
    /// is held, and again when a failed one is due. Run it only where blobs are held
    /// (`--ram-ack-bytes` over 0).
    pub async fn run_drain(&self) -> std::convert::Infallible {
        loop {
            let report = self.drain_held(Instant::now()).await;
            if report.claimed == PASS_BLOBS {
                continue;
            }
            // Registered after the pass: a blob held meanwhile left a wake-up behind.
            let kicked = self.ram.kick.notified();
            match self.held.next_due() {
                Some(due) => {
                    let _ = tokio::time::timeout_at(due, kicked).await;
                }
                None => kicked.await,
            }
        }
    }

    /// The drain a graceful shutdown makes: passes, not waiting out a backoff, until
    /// nothing is held or `deadline` has passed. A blob that still fails at the end stays
    /// held and is lost to the restart; the answer says how many.
    pub async fn drain_for_shutdown(&self, deadline: Duration) -> ShutdownDrain {
        let work = async {
            while !self.held.is_empty() {
                let report = self.drain_pass(Instant::now(), true).await;
                if report.claimed == 0 || report.failed > 0 {
                    tokio::time::sleep(SHUTDOWN_RETRY).await;
                }
            }
        };
        match tokio::time::timeout(deadline, work).await {
            Ok(()) => ShutdownDrain::Drained,
            Err(_) => {
                let left = self.held.unwritten(Instant::now());
                ShutdownDrain::TimedOut {
                    blobs: left.blobs,
                    bytes: left.bytes,
                }
            }
        }
    }

    /// Test seam: the state of `digest`'s drain, if it is held.
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn drain_state_for_test(&self, digest: &Digest) -> Option<super::held::DrainState> {
        self.held.state(digest)
    }

    /// Test seam: the digest is held now.
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn is_held_for_test(&self, digest: &Digest) -> bool {
        self.held.has(digest)
    }
}
