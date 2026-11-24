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
//! - **Directories are cached by content** ([`DirCache`]). A walk that reaches a Directory
//!   whose decoded content it holds does not read it, and does not take it for held
//!   either: the digest goes into the FindMissing calls the walk makes for its files
//!   (batched the same way, [`FILES_PER_ASK`] a call), and a digest the store reports
//!   missing is exactly an absent Directory: the loss handling is the same, and the
//!   verified memory is cleared. So a blob that was evicted or lost is still found, and
//!   an Execute never runs on inputs the store does not hold. The walk goes on below a
//!   cached Directory before the store has answered; if the answer is that it is gone,
//!   what was found below it (files, Directories, failures) is dropped from the report,
//!   so the answer is the one a walk that stopped there gives. Content not cached is
//!   read, once for all the walks that want it at the same time. A Directory a client
//!   uploaded through the frontend's packs is in the cache from the moment it was stored
//!   (`known_dirs`), so the Execute that names a tree just uploaded reads none of it; it is
//!   asked about with the files like any cached Directory. The verified memory
//!   above still skips even the FindMissing, within [`VERIFIED_TTL`].
//! - **Deterministic.** What the walk answers does not depend on which read finishes
//!   first. Directories that are not held and files that are missing are sorted sets.
//!   When a read fails for another reason (the node cannot say, a Directory does not
//!   decode), the walk goes on with everything not under that Directory, so the set of
//!   Directories it reaches is the same every time, and answers with the failure of the
//!   smallest digest among them. The FindMissing calls are answered in order, so the
//!   first of them to fail is the one reported. (With subtrees remembered, the
//!   Directories a walk reaches also depend on what earlier walks verified; the
//!   answer for a store that has not changed does not.)
//! - **What an Execute hands to the run it starts.** The run's preparation walks the same
//!   tree again (`prepare_run::closure`), with the names the Action's files have, through its
//!   own cache of named Directories ([`DirectoryCache`]). Once a preparer over the same
//!   backend has given [`Trees::keep_into`] that cache, every Directory this walk reads is
//!   also kept there, decoded with its names, so the preparation reads none of them again.
//!   And when an Execute's check finds the Action's Command and every blob of its tree held,
//!   it records when ([`Trees::vouch`], by the Action's digest); the preparation of that
//!   Action takes the record ([`Trees::vouched`], once) and does not ask the store about the
//!   Command and the files again if the record is younger than [`VERIFIED_TTL`], the same
//!   bound as for the subtrees remembered above. A record nobody takes is pushed out by
//!   newer ones ([`VOUCHED_ENTRIES`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_pack::ReadTimes;
use kbf_storage::StorageError;
use kbf_types::Digest;
use tokio::time::Instant;

use crate::dir_cache::{DirCache, Lookup};
use crate::input_tree::{Decoded, Walked, decode};
use crate::known_dirs::{KnownDirs, keep_named};
use crate::prepare_run::DirectoryCache;

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

/// The most Actions [`Trees`] keeps an Execute's finding for until their run's preparation
/// takes it (about 70 bytes each). A finding is taken within moments of being made; one that
/// is not (the Execute joined a running twin, or was refused) waits until this many newer
/// ones push it out, or is past [`VERIFIED_TTL`] when taken.
pub(crate) const VOUCHED_ENTRIES: usize = 1 << 14;

type Answer<T> = Result<T, StorageError>;

/// What the input check reads: a blob, and which of some files are held.
pub(crate) trait Blobs: Send + Sync {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>>;
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>>;

    /// [`Blobs::read_blob`], with how long a pack index lookup and a ranged read of a pack
    /// took within it: neither for a store without packs.
    fn read_timed<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, (Answer<Bytes>, ReadTimes)> {
        Box::pin(async move { (self.read_blob(digest).await, ReadTimes::default()) })
    }

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
    /// What the Directories decode to, whatever became of their subtrees: by digest, so
    /// it never goes stale, and no claim that anything is held; and where each Directory a
    /// walk reads is also kept with its names, for the preparation of the run an Execute
    /// starts ([`Trees::keep_into`]). Shared with the frontend's packs, whose writes put the
    /// Directories clients upload in it ([`Trees::sharing`]).
    known: Arc<KnownDirs>,
    /// When an Execute found the Command and every input of an Action held ([`Trees::vouch`]).
    vouched: Mutex<Vouched>,
}

/// The Executes' findings not yet taken, by the Action's digest, with the order they were
/// made in: a finding made again for the same Action replaces the one before.
struct Vouched {
    /// action -> (when its inputs were found held, its place in `order`)
    found: HashMap<Digest, (Instant, u64)>,
    /// Oldest first; a place whose finding was taken or replaced is passed over.
    order: VecDeque<(u64, Digest)>,
    next: u64,
    capacity: usize,
}

impl Vouched {
    fn new(capacity: usize) -> Self {
        Self {
            found: HashMap::new(),
            order: VecDeque::new(),
            next: 0,
            capacity,
        }
    }

    /// Records that the inputs of `action` were found held at `at`, pushing out the oldest
    /// places beyond the capacity.
    fn put(&mut self, action: Digest, at: Instant) {
        let place = self.next;
        self.next += 1;
        self.found.insert(action, (at, place));
        self.order.push_back((place, action));
        let over = |order: &mut VecDeque<(u64, Digest)>, capacity| {
            (order.len() > capacity)
                .then(|| order.pop_front())
                .flatten()
        };
        while let Some((place, action)) = over(&mut self.order, self.capacity) {
            if self
                .found
                .get(&action)
                .is_some_and(|(_, held)| *held == place)
            {
                self.found.remove(&action);
            }
        }
    }

    /// The finding for `action`, which is then gone.
    fn take(&mut self, action: &Digest) -> Option<Instant> {
        self.found.remove(action).map(|(at, _)| at)
    }
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
            known: Arc::default(),
            vouched: Mutex::new(Vouched::new(VOUCHED_ENTRIES)),
        }
    }

    /// The same, with a content cache of at most `bytes` ([`crate::dir_cache::DIR_CACHE_BYTES`] by default;
    /// zero keeps no content and shares no reads).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_dir_cache(mut self, bytes: u64) -> Self {
        self.known = Arc::new(KnownDirs::new(DirCache::new(bytes)));
        self
    }

    /// The default memory, over `known`: the Directories other walks over it read and that
    /// clients uploaded through the packs that share it.
    #[must_use]
    pub(crate) fn sharing(known: Arc<KnownDirs>) -> Self {
        Self {
            known,
            ..Self::default()
        }
    }

    /// The decoded Directories this memory's walks take before reading: for tests.
    #[cfg(test)]
    pub(crate) fn dirs(&self) -> &DirCache {
        &self.known.dirs
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
        // No floor check: `distrust` clears the entries, and `record` refuses any that
        // began before the floor, so an entry here never predates it.
        inner.recency.remove(&seq);
        if now.saturating_duration_since(at) >= self.ttl {
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
        // The memory is under the epoch the walk's lookups adopted; another epoch in
        // between means this walk's findings are for a map that is gone.
        if inner.epoch != Some(epoch) || inner.floor.is_some_and(|floor| at < floor) {
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
        let excess = inner.dirs.len().saturating_sub(self.capacity);
        for _ in 0..excess {
            let oldest = inner.recency.pop_first();
            oldest.into_iter().for_each(|(_, dir)| {
                inner.dirs.remove(&dir);
            });
        }
    }

    /// A walk found something absent: a blob went away (or was never there), and a
    /// remembered subtree may rest on it. Forgets everything, the Executes' findings not yet
    /// taken too, and refuses a verification that began before now, whenever it ends.
    fn distrust(&self, now: Instant) {
        let mut inner = self.lock();
        inner.dirs.clear();
        inner.recency.clear();
        inner.floor = Some(now);
        // Cleared with the floor held, so a finding `vouch` is putting meanwhile either sees the
        // floor or is cleared.
        self.vouches().found.clear();
    }

    /// Directories remembered now: for tests.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().dirs.len()
    }

    /// From now on, every Directory a walk reads is also kept in `cache`, decoded with its
    /// names, for the run preparation's walk of the same tree. A later cache replaces it.
    pub(crate) fn keep_into(&self, cache: Arc<DirectoryCache>) {
        self.known.keep_into(cache);
    }

    fn vouches(&self) -> MutexGuard<'_, Vouched> {
        // Map and queue edits that leave them agreeing before anything can panic.
        self.vouched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An Execute found the Command and every blob of the input tree of `action` held, by
    /// reads and FindMissing calls that all began at `at` or later. A finding that began
    /// before a walk saw something absent ([`Trees::distrust`]) is not believed, as for
    /// [`Trees::record`].
    pub(crate) fn vouch(&self, action: Digest, at: Instant) {
        // The floor is held while the finding is put, as `distrust` holds it while it clears.
        let inner = self.lock();
        if inner.floor.is_some_and(|floor| at < floor) {
            return;
        }
        self.vouches().put(action, at);
        drop(inner);
    }

    /// Whether, at `now`, an Execute's finding for `action` is younger than the time a check
    /// is believed for. The finding is taken: the next preparation of the Action, its next
    /// attempt included, checks for itself unless another Execute has found it again.
    pub(crate) fn vouched(&self, action: &Digest, now: Instant) -> bool {
        let found = self.vouches().take(action);
        found.is_some_and(|at| now.saturating_duration_since(at) < self.ttl)
    }
}

/// The input check over a store: every Directory under `root` is taken from `trees`'
/// content cache, or read ([`DIRS_IN_FLIGHT`] at once), unless `trees` remembers it
/// verified; the files they name, and the cached Directories themselves, are asked about
/// in batches. A Directory the store does not hold, or holds but cannot read, is
/// reported absent and nothing under it is looked at; the report lists absent
/// directories (sorted) before absent files (sorted), as `Cache::walk_inputs` does.
///
/// # Errors
/// The store could not answer, or a Directory it holds does not decode.
pub(crate) async fn walk(storage: &dyn Blobs, root: Digest, trees: &Trees) -> Answer<Walked> {
    walk_bounded(storage, root, DIRS_IN_FLIGHT, trees).await
}

/// How a Directory came to a walk that did not find it cached.
enum Fetched {
    /// Read from the store by this walk; the entries that pushed out of the cache.
    Read(Arc<Decoded>, u64),
    /// Read by another walk while this one waited.
    Merged(Arc<Decoded>),
}

/// One Directory fetched: which, how long its read took and its steps in the packs (`None`
/// and none if this walk did not read), what came of it, and how many dependent reads the
/// walk had made on the way to it, this one not counted.
struct Fetch {
    dir: Digest,
    took: Option<Duration>,
    steps: ReadTimes,
    outcome: Answer<Fetched>,
    rounds: u64,
}

/// Reads `dir` for everyone who wants it: leads the read when `first` says to, or waits
/// for the walk that does and looks again.
async fn fetch<'a>(
    storage: &'a dyn Blobs,
    cache: &'a DirCache,
    keep: Option<&'a DirectoryCache>,
    (dir, rounds): (Digest, u64),
    first: Lookup<'a>,
) -> Fetch {
    let mut look = first;
    loop {
        match look {
            Lookup::Cached(decoded) => {
                return Fetch {
                    dir,
                    took: None,
                    steps: ReadTimes::default(),
                    outcome: Ok(Fetched::Merged(decoded)),
                    rounds,
                };
            }
            Lookup::Follow(gate) => {
                // Closed, never given a permit: this returns when the read ends.
                let _ended = gate.acquire().await;
                look = cache.lookup(&dir);
            }
            Lookup::Lead(lead) => {
                let began = Instant::now();
                let (blob, steps) = storage.read_timed(&dir).await;
                let took = began.elapsed();
                let outcome = match blob {
                    Err(e) => Err(e),
                    Ok(bytes) => decode(&dir, &bytes)
                        .map(|decoded| {
                            if let Some(keep) = keep {
                                keep_named(keep, dir, bytes);
                            }
                            let decoded = Arc::new(decoded);
                            Fetched::Read(Arc::clone(&decoded), lead.finish(decoded))
                        })
                        .map_err(StorageError::from),
                };
                return Fetch {
                    dir,
                    took: Some(took),
                    steps,
                    outcome,
                    rounds,
                };
            }
        }
    }
}

/// What the walk has found so far, and where it goes next.
struct Frontier {
    seen: BTreeSet<Digest>,
    /// Each Directory to take, with the dependent reads the walk made on the way to it.
    queue: VecDeque<(Digest, u64)>,
    /// The files named by the Directories taken.
    files: BTreeSet<Digest>,
    /// The Directories taken and what each names.
    graph: HashMap<Digest, Arc<Decoded>>,
}

impl Frontier {
    /// `dir` is held, and `decoded` is what it names, reached after `rounds` dependent reads:
    /// its files are to be asked about and its subdirectories to be taken.
    fn take(&mut self, dir: Digest, decoded: Arc<Decoded>, rounds: u64) {
        self.files.extend(decoded.files.iter().copied());
        let seen = &mut self.seen;
        self.queue.extend(
            decoded
                .dirs
                .iter()
                .copied()
                .filter(|d| seen.insert(*d))
                .map(|d| (d, rounds)),
        );
        self.graph.insert(dir, decoded);
    }
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
    let cache = &trees.known.dirs;
    let keep = trees.known.kept();
    let mut found = Frontier {
        seen: BTreeSet::from([root]),
        queue: VecDeque::from([(root, 0)]),
        files: BTreeSet::new(),
        graph: HashMap::new(),
    };
    let mut reading = FuturesUnordered::new();
    let mut absent_dirs = BTreeSet::new();
    // The Directories taken from the cache: held as far as the store has said, which it
    // has not yet.
    let mut cached = BTreeSet::new();
    let mut held_dirs = Vec::new();
    let mut walked = Walked::default();
    // The oldest verification a skipped subtree rests on.
    let mut verified_at = begun;
    let mut failures: Vec<(Digest, StorageError)> = Vec::new();
    loop {
        while reading.len() < reads_in_flight {
            let Some((dir, rounds)) = found.queue.pop_front() else {
                break;
            };
            if let Some(at) = epoch.and_then(|e| trees.fresh(e, &dir, Instant::now())) {
                walked.dirs_skipped += 1;
                verified_at = verified_at.min(at);
                continue;
            }
            match cache.lookup(&dir) {
                Lookup::Cached(decoded) => {
                    walked.dir_cache_hits += 1;
                    cached.insert(dir);
                    held_dirs.push(dir);
                    found.take(dir, decoded, rounds);
                }
                first => {
                    if matches!(first, Lookup::Lead(_)) {
                        walked.dir_cache_misses += 1;
                    }
                    let fetching = fetch(storage, cache, keep.as_deref(), (dir, rounds), first);
                    reading.push(fetching);
                }
            }
        }
        let Some(Fetch {
            dir,
            took,
            steps,
            outcome,
            rounds,
        }) = reading.next().await
        else {
            break;
        };
        // A read, or a wait for another walk's read: the Directories under it are one round
        // further from the root.
        let rounds = rounds + 1;
        walked.depth = walked.depth.max(rounds);
        walked.read_times.extend(took);
        walked.index_times.extend(steps.index);
        walked.object_times.extend(steps.objects);
        match outcome {
            Err(StorageError::NotFound(_) | StorageError::Unreachable(_)) => {
                absent_dirs.insert(dir);
            }
            Err(e) => failures.push((dir, e)),
            Ok(fetched) => {
                held_dirs.push(dir);
                let decoded = match fetched {
                    Fetched::Read(decoded, evicted) => {
                        walked.dirs_read += 1;
                        walked.dir_cache_evicted += evicted;
                        decoded
                    }
                    Fetched::Merged(decoded) => {
                        walked.dir_cache_hits += 1;
                        walked.dir_cache_merged += 1;
                        decoded
                    }
                };
                found.take(dir, decoded, rounds);
            }
        }
    }
    walked.dirs_time = begun.elapsed();
    walked.dir_cache_bytes = cache.bytes();
    walked.dirs_present_checked = cached.len() as u64;
    // Nothing is asked of a walk that failed unless a cached Directory might hide the
    // failure (below one that turns out to be gone).
    if cached.is_empty()
        && let Some((_, e)) = smallest(&mut failures, |_| true)
    {
        return Err(e);
    }
    // A Directory is asked about once whatever names it. The cached ones are asked about
    // with the files: a cached Directory is trusted only if the store still holds it.
    let mut asking: Vec<Digest> = found.files.iter().copied().collect();
    asking.extend(cached.iter().filter(|d| !found.files.contains(d)));
    let files_began = Instant::now();
    // Built before the stream: a closure inside it makes the future not `Send` for every
    // lifetime of a chunk.
    let asks: Vec<BoxFuture<'_, Answer<Vec<Digest>>>> = asking
        .chunks(FILES_PER_ASK)
        .map(|chunk| storage.find_missing(chunk))
        .collect();
    let asked: Vec<Vec<Digest>> = futures::stream::iter(asks)
        .buffered(ASKS_IN_FLIGHT)
        .try_collect()
        .await?;
    walked.files_time = files_began.elapsed();
    let missing: BTreeSet<Digest> = asked.into_iter().flatten().collect();
    let lost: BTreeSet<Digest> = cached.intersection(&missing).copied().collect();
    let (absent_dirs, absent_files) = if lost.is_empty() {
        if let Some((_, e)) = smallest(&mut failures, |_| true) {
            return Err(e);
        }
        (absent_dirs, missing)
    } else {
        // A cached Directory the store lost is absent exactly as one the walk failed to
        // read is, and nothing under it counts: what the walk saw below it (it went on
        // under the cached content, not knowing) is dropped.
        let (dirs, files) = below_lost(root, &found.graph, &lost);
        if let Some((_, e)) = smallest(&mut failures, |d| dirs.contains(d)) {
            return Err(e);
        }
        let absent_dirs = absent_dirs
            .into_iter()
            .chain(lost.iter().copied())
            .filter(|d| dirs.contains(d))
            .collect();
        let absent_files = missing
            .into_iter()
            .filter(|f| files.contains(f) && !lost.contains(f))
            .collect();
        (absent_dirs, absent_files)
    };
    let absent: Vec<Digest> = absent_dirs.into_iter().chain(absent_files).collect();
    if let Some(epoch) = epoch {
        if !absent.is_empty() {
            trees.distrust(Instant::now());
        } else if storage.epoch() == Some(epoch) {
            // Every Directory taken has its files held and its Directories taken or
            // remembered, and the store has said it holds the cached ones: nothing was
            // absent and nothing failed.
            trees.record(epoch, &held_dirs, verified_at);
        }
    }
    // The whole tree was seen only if no subtree was skipped and nothing was absent; any
    // other total would be a lower bound that reads as a size.
    if walked.dirs_skipped == 0 && absent.is_empty() {
        walked.input_files = Some(found.files.len() as u64);
        walked.input_bytes = Some(
            found
                .files
                .iter()
                .fold(0_u64, |sum, f| sum.saturating_add(f.size_bytes)),
        );
    }
    walked.held_since = absent.is_empty().then_some(verified_at);
    walked.absent = absent;
    walked.files_checked = found.files.len() as u64;
    walked.file_asks = asking.len().div_ceil(FILES_PER_ASK) as u64;
    walked.phased = true;
    Ok(walked)
}

/// The failure of the smallest digest among those `of` accepts.
fn smallest(
    failures: &mut Vec<(Digest, StorageError)>,
    of: impl Fn(&Digest) -> bool,
) -> Option<(Digest, StorageError)> {
    failures
        .drain(..)
        .filter(|(dir, _)| of(dir))
        .min_by_key(|(dir, _)| *dir)
}

/// The Directories and the files of the tree under `root` that a walk which stopped at
/// every Directory of `lost` reaches: the lost ones are in the first set (they are
/// named), nothing under them is in either.
fn below_lost(
    root: Digest,
    graph: &HashMap<Digest, Arc<Decoded>>,
    lost: &BTreeSet<Digest>,
) -> (BTreeSet<Digest>, BTreeSet<Digest>) {
    let (mut dirs, mut files) = (BTreeSet::from([root]), BTreeSet::new());
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Some(decoded) = graph.get(&dir).filter(|_| !lost.contains(&dir)) else {
            continue;
        };
        files.extend(decoded.files.iter().copied());
        stack.extend(decoded.dirs.iter().copied().filter(|d| dirs.insert(*d)));
    }
    (dirs, files)
}

#[cfg(test)]
#[path = "input_walk_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "input_walk_memory_tests.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "input_walk_dircache_tests.rs"]
mod dircache_tests;

#[cfg(test)]
#[path = "input_walk_handoff_tests.rs"]
mod handoff_tests;

#[cfg(test)]
#[path = "input_walk_rounds_tests.rs"]
mod rounds_tests;
