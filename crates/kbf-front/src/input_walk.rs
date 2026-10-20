//! The input check over a store that is not in this process: [`walk`].
//!
//! An Execute's input root names a tree of Directory blobs and, under it, files. Over a
//! [`kbf_storage::Storage`] or a routed farm the frontend holds nothing of its own, so
//! every Directory of the tree is a network read, and a toolchain tree is a thousand of
//! them. The walk keeps [`DIRS_IN_FLIGHT`] of those reads open at once, whatever shape
//! the tree has:
//!
//! - **Pipelined, not level by level.** A read that finishes puts the directories under
//!   it in the queue and the next read starts at once; a level that is narrower than the
//!   bound does not leave slots idle behind a straggler of the level before. A tree of
//!   depth D whose levels are all within the bound takes D round trips.
//! - **Each digest is read once.** A digest names its content, so a subtree that many
//!   parents name (Merkle trees repeat them) is read when first reached and never again.
//! - **Bounded.** At most [`DIRS_IN_FLIGHT`] reads are open and at most
//!   [`ASKS_IN_FLIGHT`] FindMissing calls; what the walk keeps is the digests of the tree
//!   (the seen set, the queue and the files), not its blobs.
//! - **Verified subtrees are remembered** ([`Trees`]). A walk that found a Directory and
//!   everything under it held records the digests, with the time it began and the map
//!   epoch ([`Blobs::epoch`]) it ran under; a later walk that reaches one of them within
//!   [`VERIFIED_TTL`], under the same epoch, skips it: no read, no FindMissing. Content
//!   addressing means an entry never goes stale in what it says a Directory contains;
//!   only whether its blobs are still held can change, and that is bounded three ways: by
//!   the time to live (the longest a blob that went away can still be taken for held,
//!   as for the router's closure check), by the epoch (a new map generation or a
//!   restarted member drops everything), and by [`Trees::distrust`] (a walk that finds
//!   anything absent has seen a loss, so nothing verified before then is believed).
//!   An entry is written only when the whole walk found nothing absent and nothing
//!   failed, never from a partial result.
//! - **Deterministic.** What the walk answers does not depend on which read finishes
//!   first. Directories that are not held and files that are missing are sorted sets.
//!   When a read fails for another reason (the node cannot say, a Directory does not
//!   decode), the walk goes on with everything not under that Directory, so the set of
//!   Directories it reaches is the same every time, and answers with the failure of the
//!   smallest digest among them. The FindMissing calls are answered in order, so the
//!   first of them to fail is the one reported. (With subtrees remembered, the
//!   Directories a walk reaches also depend on what earlier walks verified; the
//!   answer for a store that has not changed does not.)

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_storage::StorageError;
use kbf_types::Digest;
use tokio::time::Instant;

use crate::input_tree::{Walked, decode};

/// The most Directory blobs one [`walk`] reads from the store at once. The tree of a
/// request is the only bound on its fan-out, so this is what keeps a few hundred
/// Executes arriving together from opening a few hundred thousand reads.
pub(crate) const DIRS_IN_FLIGHT: usize = 32;

/// The most digests one FindMissing of [`walk`] names: a message well under the gRPC
/// limit, with a call per few thousand files of a tree.
pub(crate) const FILES_PER_ASK: usize = 4096;

/// The most FindMissing calls of one [`walk`] open at once.
pub(crate) const ASKS_IN_FLIGHT: usize = 4;

/// How long a verified Directory is believed. Shared with the router's closure check
/// (`kbf_route::DEFAULT_CLOSURE_TTL`): a build's actions arrive within seconds of each
/// other, so a toolchain tree is walked about once per this long, and a blob that went
/// away is taken for held for no longer than this.
pub(crate) const VERIFIED_TTL: Duration = Duration::from_secs(45);

/// The most Directories [`Trees`] remembers (about 100 bytes each): a farm's worth of
/// toolchains and sources, with room.
pub(crate) const VERIFIED_ENTRIES: usize = 1 << 18;

type Answer<T> = Result<T, StorageError>;

/// What the input check reads: a blob, and which of some files are held.
pub(crate) trait Blobs: Send + Sync {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>>;
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>>;

    /// What a "held" answer rests on, as a number that changes when it may have changed
    /// (see `kbf_route::Router::epoch`); `None` when the store cannot say, and nothing
    /// is then remembered.
    fn epoch(&self) -> Option<u64> {
        None
    }
}

/// The Directories whose whole subtree was found held, and when.
pub(crate) struct Trees {
    inner: Mutex<Remembered>,
    capacity: usize,
    ttl: Duration,
}

#[derive(Default)]
struct Remembered {
    /// The epoch every entry was verified under.
    epoch: Option<u64>,
    /// Verifications that began before this are not believed ([`Trees::distrust`]).
    floor: Option<Instant>,
    /// digest -> (when its verifying walk began, its place in `recency`)
    dirs: HashMap<Digest, (Instant, u64)>,
    /// Least recently used first.
    recency: BTreeMap<u64, Digest>,
    next: u64,
}

impl Remembered {
    /// Drops everything and adopts `epoch`.
    fn reset(&mut self, epoch: u64) {
        self.dirs.clear();
        self.recency.clear();
        self.epoch = Some(epoch);
    }
}

impl Default for Trees {
    fn default() -> Self {
        Self::new(VERIFIED_ENTRIES, VERIFIED_TTL)
    }
}

impl Trees {
    /// A memory of at most `capacity` Directories (least recently used out first), each
    /// believed for at most `ttl`. Zero capacity remembers nothing.
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Remembered::default()),
            capacity,
            ttl,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Remembered> {
        // Every change leaves the two maps agreeing before it can panic.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// When the walk that verified `dir` began, if `dir` and everything under it is still
    /// believed held under `epoch`. A hit counts as a use.
    fn fresh(&self, epoch: u64, dir: &Digest, now: Instant) -> Option<Instant> {
        let mut inner = self.lock();
        if inner.epoch != Some(epoch) {
            inner.reset(epoch);
            return None;
        }
        let &(at, seq) = inner.dirs.get(dir)?;
        let believed = now.saturating_duration_since(at) < self.ttl
            && inner.floor.is_none_or(|floor| at >= floor);
        inner.recency.remove(&seq);
        if !believed {
            inner.dirs.remove(dir);
            return None;
        }
        let used = inner.next;
        inner.next += 1;
        inner.recency.insert(used, *dir);
        inner.dirs.insert(*dir, (at, used));
        Some(at)
    }

    /// Remembers that `dirs`, each with everything under it, were held at `at`, a time
    /// no later than the start of any read the finding rests on.
    fn record(&self, epoch: u64, dirs: &[Digest], at: Instant) {
        if self.capacity == 0 {
            return;
        }
        let mut inner = self.lock();
        if inner.epoch != Some(epoch) {
            inner.reset(epoch);
        }
        if inner.floor.is_some_and(|floor| at < floor) {
            return;
        }
        for dir in dirs {
            let used = inner.next;
            inner.next += 1;
            inner.recency.insert(used, *dir);
            if let Some((_, old)) = inner.dirs.insert(*dir, (at, used)) {
                inner.recency.remove(&old);
            }
        }
        while inner.dirs.len() > self.capacity {
            let Some((_, oldest)) = inner.recency.pop_first() else {
                break;
            };
            inner.dirs.remove(&oldest);
        }
    }

    /// A walk found something absent: a blob went away (or was never there), and a
    /// remembered subtree may rest on it. Forgets everything, and refuses a verification
    /// that began before now, whenever it ends.
    fn distrust(&self, now: Instant) {
        let mut inner = self.lock();
        inner.dirs.clear();
        inner.recency.clear();
        inner.floor = Some(now);
    }

    /// Directories remembered now: for tests.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().dirs.len()
    }
}

/// The input check over a store: every Directory under `root` is read
/// ([`DIRS_IN_FLIGHT`] at once) unless `trees` remembers it verified, and the files they
/// name are asked about in batches. A Directory the store does not hold, or holds but
/// cannot read, is reported absent and nothing under it is looked at; the report lists
/// absent directories (sorted) before absent files (sorted), as `Cache::walk_inputs`
/// does.
///
/// # Errors
/// The store could not answer, or a Directory it holds does not decode.
pub(crate) async fn walk(storage: &dyn Blobs, root: Digest, trees: &Trees) -> Answer<Walked> {
    walk_bounded(storage, root, DIRS_IN_FLIGHT, trees).await
}

/// [`walk`] with at most `reads_in_flight` Directory reads open (at least one).
pub(crate) async fn walk_bounded(
    storage: &dyn Blobs,
    root: Digest,
    reads_in_flight: usize,
    trees: &Trees,
) -> Answer<Walked> {
    let reads_in_flight = reads_in_flight.max(1);
    let begun = Instant::now();
    // Read before the first read: the walk is remembered only under the epoch it began in.
    let epoch = storage.epoch();
    let mut seen = BTreeSet::from([root]);
    let mut queue = VecDeque::from([root]);
    let mut reading = FuturesUnordered::new();
    let (mut absent_dirs, mut files) = (BTreeSet::new(), BTreeSet::new());
    let mut held_dirs = Vec::new();
    let (mut dirs_read, mut dirs_skipped) = (0, 0);
    // The oldest verification a skipped subtree rests on.
    let mut verified_at = begun;
    let mut failure: Option<(Digest, StorageError)> = None;
    loop {
        while reading.len() < reads_in_flight {
            let Some(dir) = queue.pop_front() else { break };
            if let Some(at) = epoch.and_then(|e| trees.fresh(e, &dir, Instant::now())) {
                dirs_skipped += 1;
                verified_at = verified_at.min(at);
                continue;
            }
            reading.push(async move { (dir, storage.read_blob(&dir).await) });
        }
        let Some((dir, blob)) = reading.next().await else {
            break;
        };
        let failed = match blob {
            Err(StorageError::NotFound(_) | StorageError::Unreachable(_)) => {
                absent_dirs.insert(dir);
                None
            }
            Err(e) => Some(e),
            Ok(bytes) => {
                dirs_read += 1;
                match decode(&dir, &bytes) {
                    Ok(decoded) => {
                        held_dirs.push(dir);
                        files.extend(decoded.files.iter().copied());
                        queue.extend(decoded.dirs.iter().copied().filter(|d| seen.insert(*d)));
                        None
                    }
                    Err(e) => Some(StorageError::from(e)),
                }
            }
        };
        if let Some(e) = failed
            && failure.as_ref().is_none_or(|(was, _)| dir < *was)
        {
            failure = Some((dir, e));
        }
    }
    let dirs_time = begun.elapsed();
    if let Some((_, e)) = failure {
        return Err(e);
    }
    let files: Vec<Digest> = files.into_iter().collect();
    let files_began = Instant::now();
    // Built before the stream: a closure inside it makes the future not `Send` for every
    // lifetime of a chunk.
    let asks: Vec<BoxFuture<'_, Answer<Vec<Digest>>>> = files
        .chunks(FILES_PER_ASK)
        .map(|chunk| storage.find_missing(chunk))
        .collect();
    let asked: Vec<Vec<Digest>> = futures::stream::iter(asks)
        .buffered(ASKS_IN_FLIGHT)
        .try_collect()
        .await?;
    let files_time = files_began.elapsed();
    let absent_files: BTreeSet<Digest> = asked.into_iter().flatten().collect();
    let absent: Vec<Digest> = absent_dirs.into_iter().chain(absent_files).collect();
    if let Some(epoch) = epoch {
        if !absent.is_empty() {
            trees.distrust(Instant::now());
        } else if storage.epoch() == Some(epoch) {
            // Every Directory read has its files held and its Directories read or
            // remembered: nothing was absent and nothing failed.
            trees.record(epoch, &held_dirs, verified_at);
        }
    }
    Ok(Walked {
        absent,
        dirs_read,
        dirs_skipped,
        files_checked: files.len() as u64,
        file_asks: files.len().div_ceil(FILES_PER_ASK) as u64,
        dirs_time,
        files_time,
        phased: true,
    })
}

#[cfg(test)]
#[path = "input_walk_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "input_walk_memory_tests.rs"]
mod memory_tests;
