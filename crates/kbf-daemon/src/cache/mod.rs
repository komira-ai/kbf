//! The worker's input cache (`docs/design/worker-cache.md#6-the-cache`): a
//! content-addressed directory of read-only files on the node's disk, which the
//! container driver hard-links into each lease's input root.
//!
//! - **One owner.** [`Cache::open`] takes an exclusive `flock` on `<dir>/lock` before
//!   it touches anything else; a second daemon is refused ([`Disabled::Locked`]) and
//!   runs uncached. The lock file is never unlinked.
//! - **Inserts** are fetched and hashed into `tmp/`, made read-only (0444, 0555 for an
//!   executable) with the fixed mtime [`FIXED_MTIME`], and renamed into `blobs/` with
//!   `RENAME_NOREPLACE` (`fill`). Nothing under `blobs/` is ever opened for writing.
//! - **The index** of keys, sizes and last uses is in memory (`index`). The files a
//!   previous run left in `blobs/` are not in it: a fill that finds one at its key's
//!   name re-hashes it and adopts it, or unlinks it, before it fetches.
//! - **Every key's slot** in the fill map serialises what changes that key's name and
//!   index entry: fills and the evictor (`evict`).
//! - **Every entry is stat-checked** on every hit (`verify`), and hashed when it is
//!   inserted or adopted.
//!
//! The cache never fails a lease that would run without it: an I/O error in the cache
//! hands the lease the fetched bytes to write itself, as without a cache.

mod evict;
mod fill;
mod index;
mod layout;
pub(crate) mod lease;
mod verify;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub use fill::InputStats;
pub use layout::{FIXED_MTIME, Key};

use crate::cas::CasError;
use index::Index;

/// Unix seconds now: the clock the cache dates last uses with. Tests give a fake one.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system clock as a [`Clock`].
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    })
}

/// A place in the cache's work where a test may look, or hold the work until it lets
/// it go. Every point is reached on a blocking thread, never on an async one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point<'a> {
    /// A fill's temporary file is written, read-only and dated, and not yet renamed
    /// into `blobs/`.
    Filled(&'a Key),
    /// The evictor took the key's slot and removed it from the index; its name is not
    /// unlinked yet.
    Evicting(&'a Key),
}

/// Where a test watches the cache's work: called at each [`Point`].
pub trait Hooks: Send + Sync {
    fn at(&self, point: Point<'_>);
}

/// How the cache makes a hard link (`std::fs::hard_link`): a test gives one that fails
/// as a full link count or another filesystem would.
pub type LinkFn = dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync;

/// The hooks of a daemon: none.
#[derive(Debug)]
pub struct NoHooks;

impl Hooks for NoHooks {
    fn at(&self, _: Point<'_>) {}
}

/// How a cache is opened and run.
#[derive(Clone)]
pub struct CacheConfig {
    /// `--cache-dir`: the directory the cache owns. It must exist.
    pub dir: PathBuf,
    /// The lease scratch directory, whose filesystem the cache should share.
    pub scratch: PathBuf,
    /// `--cache-max-bytes`: the most bytes of entries kept.
    pub max_bytes: u64,
    /// `--cache-min-free-bytes`: below this much free space on the filesystem,
    /// entries no lease links are evicted. 0 is no floor.
    pub min_free_bytes: u64,
    /// `--cache-link-concurrency`: how many files of a lease are linked or filled at
    /// once.
    pub link_concurrency: usize,
    pub clock: Clock,
    pub hooks: Arc<dyn Hooks>,
    /// Links a lease's name to an entry, or to another name of the lease.
    pub link: Arc<LinkFn>,
    /// The most links an entry may have; `None` asks the filesystem
    /// (`pathconf(_PC_LINK_MAX)`).
    pub link_max: Option<u64>,
}

impl CacheConfig {
    /// A cache in `dir` for leases under `scratch`, at most `max_bytes`: no free-space
    /// floor, 64 links at once, the system clock, no hooks.
    #[must_use]
    pub fn new(dir: PathBuf, scratch: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            scratch,
            max_bytes,
            min_free_bytes: 0,
            link_concurrency: 64,
            clock: system_clock(),
            hooks: Arc::new(NoHooks),
            link: Arc::new(|from: &Path, to: &Path| std::fs::hard_link(from, to)),
            link_max: None,
        }
    }
}

/// Why the daemon runs uncached (`cache_disabled_reason`). The daemon warns and serves
/// either way.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Disabled {
    /// Another daemon holds the directory's lock.
    #[error("locked: another daemon owns the cache directory")]
    Locked,
    /// The directory is missing, cannot be written, or holds no usable layout.
    #[error("unusable: {0}")]
    Unusable(String),
    /// The directory's name is one a lease sweep removes.
    #[error(
        "lease-name: a cache directory named kbf-lease-* or lease-* is removed by lease sweeps"
    )]
    LeaseName,
    /// `--cache-dir` without `--cache-max-bytes`.
    #[error("no-budget: --cache-dir needs --cache-max-bytes")]
    NoBudget,
    /// The Linux native driver: actions run as the cache's owner and could write it.
    #[error("native-same-uid: native actions run as the daemon's user and could write the cache")]
    NativeSameUid,
    /// Podman's storage is not one the cache's overlay checks passed on.
    #[error("graph-driver: {0}")]
    GraphDriver(String),
}

impl Disabled {
    /// The word reported as `cache_disabled_reason`.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Unusable(_) => "unusable",
            Self::LeaseName => "lease-name",
            Self::NoBudget => "no-budget",
            Self::NativeSameUid => "native-same-uid",
            Self::GraphDriver(_) => "graph-driver",
        }
    }
}

/// How a lease gets a cached file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkMode {
    /// A hard link to the entry.
    Hardlink,
    /// A copy of the entry: the cache is on another filesystem than the scratch
    /// directory. It still saves the network fetch.
    Copy,
}

impl std::fmt::Display for LinkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hardlink => "hardlink",
            Self::Copy => "copy",
        })
    }
}

/// The node's cache counters (section 7 of the design, the ones C1 keeps).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Bytes and entries in the index.
    pub bytes: u64,
    pub entries: u64,
    pub evictions: u64,
    /// Entries that failed their stat check or their re-hash.
    pub verify_failures: u64,
    /// Free-space rounds that ran out of entries no lease links.
    pub floor_unreachable: u64,
}

#[derive(Debug, Default)]
struct Counters {
    evictions: AtomicU64,
    verify_failures: AtomicU64,
    floor_unreachable: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// A key's slot, held by a fill or the evictor: what its waiters learn when it is
/// released (a fill's error, if it failed). Whoever held it, a waiter then looks at the
/// index again: the holder leaves the key's name and index entry in agreement.
#[derive(Debug)]
struct Slot {
    done: tokio::sync::watch::Receiver<Option<Option<CasError>>>,
}

/// What the cache's mutex guards: the index, the slots and the pins.
#[derive(Debug, Default)]
struct State {
    index: Index,
    slots: HashMap<Key, Slot>,
    pins: HashMap<Key, u32>,
}

/// The cache as one daemon owns it.
pub struct Cache {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("dir", &self.inner.config.dir)
            .finish_non_exhaustive()
    }
}

struct Inner {
    config: CacheConfig,
    blobs: PathBuf,
    tmp: PathBuf,
    /// Holds the exclusive flock while the cache is open.
    _lock: File,
    link_mode: LinkMode,
    link_max: u64,
    uid: u32,
    state: Mutex<State>,
    counters: Counters,
    /// Names the temporary files of fills.
    next_tmp: AtomicU64,
}

impl Cache {
    /// Opens the cache in `config.dir`, blocking: takes the lock, then clears what a
    /// previous run left in `tmp/`. The index starts empty.
    ///
    /// # Errors
    /// Why the cache cannot be used; the daemon then runs uncached.
    pub fn open(config: CacheConfig) -> Result<Arc<Self>, Disabled> {
        if config
            .dir
            .file_name()
            .is_some_and(|name| layout::swept_name(Path::new(name)))
        {
            return Err(Disabled::LeaseName);
        }
        let opened = layout::open(&config.dir, &config.scratch, (config.clock)())?;
        let link_mode = if opened.same_filesystem {
            LinkMode::Hardlink
        } else {
            let dir = config.dir.display();
            tracing::warn!("cache: {dir} is not on the scratch filesystem; leases get copies");
            LinkMode::Copy
        };
        let inner = Arc::new(Inner {
            link_max: config
                .link_max
                .unwrap_or_else(|| layout::link_max(&opened.blobs)),
            uid: rustix::process::geteuid().as_raw(),
            blobs: opened.blobs,
            tmp: opened.tmp,
            _lock: opened.lock,
            link_mode,
            state: Mutex::new(State::default()),
            counters: Counters::default(),
            next_tmp: AtomicU64::new(0),
            config,
        });
        Ok(Arc::new(Self { inner }))
    }

    /// The directory the cache owns (`--cache-dir`).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.config.dir
    }

    /// How leases get cached files.
    #[must_use]
    pub fn link_mode(&self) -> LinkMode {
        self.inner.link_mode
    }

    /// The most hard links an entry may get before leases get copies instead.
    #[must_use]
    pub fn link_max(&self) -> u64 {
        self.inner.link_max
    }

    /// How many files of a lease are linked or filled at once.
    #[must_use]
    pub fn link_concurrency(&self) -> usize {
        self.inner.config.link_concurrency.max(1)
    }

    /// The node's counters now.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let inner = &*self.inner;
        let state = inner.state();
        let c = &inner.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        CacheStats {
            bytes: state.index.total(),
            entries: state.index.len() as u64,
            evictions: get(&c.evictions),
            verify_failures: get(&c.verify_failures),
            floor_unreachable: get(&c.floor_unreachable),
        }
    }

    /// Evicts down to the budget and the free-space floor now.
    pub fn evict(&self) {
        self.inner.evict();
    }

    /// Pins `keys` (each once per call) until the returned guard is dropped: the
    /// evictor skips a pinned key.
    #[must_use]
    pub fn pin(&self, keys: Vec<Key>) -> Pins {
        let mut state = self.inner.state();
        for key in &keys {
            *state.pins.entry(*key).or_default() += 1;
        }
        drop(state);
        Pins {
            inner: Arc::clone(&self.inner),
            keys,
        }
    }

    /// Whether `key` is in the index, for tests.
    #[must_use]
    pub fn holds(&self, key: &Key) -> bool {
        self.inner.state().index.get(key).is_some()
    }
}

/// Keys pinned by one lease.
pub struct Pins {
    inner: Arc<Inner>,
    keys: Vec<Key>,
}

impl Drop for Pins {
    fn drop(&mut self) {
        let mut state = self.inner.state();
        for key in &self.keys {
            let left = state.pins.get_mut(key).map(|count| {
                *count -= 1;
                *count
            });
            if left == Some(0) {
                state.pins.remove(key);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every update under these locks leaves them consistent, so a panic elsewhere
    // does not make them unusable.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn now(&self) -> u64 {
        (self.config.clock)()
    }

    fn path_of(&self, key: &Key) -> PathBuf {
        self.blobs.join(key.prefix()).join(key.name())
    }
}

#[cfg(test)]
mod tests;
