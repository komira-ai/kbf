//! A lease's way to one key's entry (`docs/design/worker-cache.md#64-concurrency-and-fills`):
//! a hit, a wait on whoever holds the key's slot, or the slot itself, under which the
//! key's name is checked, re-hashed, adopted, unlinked or filled.
//!
//! Under the slot (`settle`), the final name is looked at first. A file there that
//! passes the stat check and hashes to the key is used (adopted into the index if it
//! was not in it: inserted after the last snapshot). A file that fails either is
//! unlinked. Only then is the blob written into a new file in `tmp/`, from the other key
//! of the same blob when that one is indexed and hashes right (a local copy), else
//! from the CAS as it arrives ([`Cas::read_to`](crate::cas::Cas::read_to): never held
//! whole in memory, whatever its size), then chmodded 0444 or 0555, dated
//! [`FIXED_MTIME`], closed, and renamed into `blobs/` with `RENAME_NOREPLACE`. While
//! the slot is held nothing in the daemon makes that name, so `EEXIST` means something
//! outside did: the lease copies the checked file from `tmp/` and removes it, and the
//! key is recorded for the next adopt pass. Any other I/O error after the bytes are
//! written does the same; one before (no file in `tmp/`, a write that fails) leaves
//! the lease to fetch the blob into its own file. The cache never fails a lease that
//! would run without it.

use std::io::{ErrorKind, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{CWD, RenameFlags, Timespec, Timestamps};
use tokio::sync::watch;

use super::layout::{FIXED_MTIME, Key, remove_if_present};
use super::verify::{Found, inspect, stat_check};
use super::{Cache, Inner, Point, Slot, State, bump};
use crate::cas::CasError;

/// Where a lease gets a key's bytes, and whether the cache had them.
#[derive(Debug)]
pub(crate) struct Got {
    pub source: Source,
    /// The key was in the index (and its entry passed its checks).
    pub hit: bool,
}

/// Where a lease gets a key's bytes.
#[derive(Debug)]
pub(crate) enum Source {
    /// The entry at `path`, which has `nlink` links.
    Entry { path: PathBuf, nlink: u64 },
    /// A checked file of the key's bytes in `tmp/` that the cache could not keep: the
    /// lease's alone, copied from and then removed.
    Loose(PathBuf),
    /// Nothing: the cache could not write the bytes, and the lease fetches them into
    /// its own file.
    Fetch,
}

/// What one lease's inputs cost and where they came from (`InputStats` in the design;
/// the API carries them from C3). Counted while the lease's inputs are written.
#[derive(Debug, Default)]
pub struct InputCounts {
    pub(crate) files: AtomicU64,
    pub(crate) empty_files: AtomicU64,
    pub(crate) hits: AtomicU64,
    pub(crate) misses: AtomicU64,
    pub(crate) fill_waits: AtomicU64,
    pub(crate) verify_hashes: AtomicU64,
    pub(crate) fetched_bytes: AtomicU64,
    pub(crate) link_fallbacks: AtomicU64,
    pub(crate) links: AtomicU64,
    pub(crate) copies: AtomicU64,
}

/// [`InputCounts`] once the inputs are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Files in the input tree, each name once; empty ones are created directly.
    pub files: u64,
    pub empty_files: u64,
    /// Files (names) whose bytes the cache had, and files it had to fill.
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Keys this lease waited on another's fill for.
    pub fill_waits: u64,
    /// Entries this lease re-hashed (their first use since the start).
    pub verify_hashes: u64,
    /// Blob bytes fetched from the CAS for this lease.
    pub fetched_bytes: u64,
    /// Keys this lease got a private copy of instead of a link.
    pub link_fallbacks: u64,
    /// Files (names) that are hard links to a cache entry. Not in the `Display` line
    /// (the driver's `inputs phases:` line shows it).
    pub links: u64,
    /// Files (names) that are their own copy, not the cache entry's inode: the private
    /// ones an output path covers, and every name of a key that fell back. With
    /// `links` and `empty_files`, they add up to `files`. Not in the `Display` line.
    pub copies: u64,
}

impl InputCounts {
    /// What was counted.
    #[must_use]
    pub fn stats(&self) -> InputStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        InputStats {
            files: get(&self.files),
            empty_files: get(&self.empty_files),
            cache_hits: get(&self.hits),
            cache_misses: get(&self.misses),
            fill_waits: get(&self.fill_waits),
            verify_hashes: get(&self.verify_hashes),
            fetched_bytes: get(&self.fetched_bytes),
            link_fallbacks: get(&self.link_fallbacks),
            links: get(&self.links),
            copies: get(&self.copies),
        }
    }
}

impl std::fmt::Display for InputStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "files={} empty_files={} cache_hits={} cache_misses={} fill_waits={} \
             verify_hashes={} fetched_bytes={} link_fallbacks={}",
            self.files,
            self.empty_files,
            self.cache_hits,
            self.cache_misses,
            self.fill_waits,
            self.verify_hashes,
            self.fetched_bytes,
            self.link_fallbacks
        )
    }
}

pub(crate) use crate::cas::Fetch;

/// A held slot. Dropped, it leaves the fill map and wakes the waiters, handing them
/// the fill's error if it failed.
pub(super) struct SlotGuard<'a> {
    inner: &'a Inner,
    key: Key,
    done: watch::Sender<Option<Option<CasError>>>,
    error: Option<CasError>,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.inner.state().slots.remove(&self.key);
        self.done.send_replace(Some(self.error.take()));
    }
}

impl Inner {
    /// Takes `key`'s slot; the caller has checked that it is free.
    pub(super) fn take<'a>(&'a self, state: &mut State, key: Key) -> SlotGuard<'a> {
        let (done, waiters) = watch::channel(None);
        state.slots.insert(key, Slot { done: waiters });
        SlotGuard {
            inner: self,
            key,
            done,
            error: None,
        }
    }
}

/// What a lease does next for a key.
enum Step<'a> {
    /// The entry is indexed and verified: stat-check it and use it.
    Hit,
    /// Someone holds the slot: wait, then look again.
    Wait(watch::Receiver<Option<Option<CasError>>>),
    /// The slot is the lease's: settle the key.
    Own(SlotGuard<'a>),
}

impl Cache {
    /// `key`'s bytes for a lease: its entry, or the bytes when the cache could not keep
    /// them. `stale` says the lease found the entry gone (a link failed with `ENOENT`),
    /// so the index is not trusted for it.
    pub(crate) async fn get(
        &self,
        cas: &dyn Fetch,
        key: Key,
        stale: bool,
        counts: &InputCounts,
    ) -> Result<Got, CasError> {
        let inner = &*self.inner;
        let mut force = stale;
        loop {
            match inner.step(key, force) {
                Step::Hit => {
                    let path = inner.path_of(&key);
                    let uid = inner.uid;
                    let checked = blocking(move || stat_check(&path, &key, uid).ok()).await;
                    if let Some(nlink) = checked {
                        let path = inner.path_of(&key);
                        let source = Source::Entry { path, nlink };
                        return Ok(Got { source, hit: true });
                    }
                    force = true;
                }
                Step::Wait(mut done) => {
                    bump(&counts.fill_waits);
                    let failed = done
                        .wait_for(Option::is_some)
                        .await
                        .ok()
                        .and_then(|d| d.clone().flatten());
                    if let Some(error) = failed {
                        return Err(error);
                    }
                    force = false;
                }
                Step::Own(mut slot) => {
                    let settled = self.settle(cas, key, counts).await;
                    slot.error = settled.as_ref().err().cloned();
                    return settled;
                }
            }
        }
    }

    /// Settles `key` while the caller holds its slot: see the module docs.
    async fn settle(
        &self,
        cas: &dyn Fetch,
        key: Key,
        counts: &InputCounts,
    ) -> Result<Got, CasError> {
        let inner = &*self.inner;
        let indexed = inner.state().index.get(&key).is_some();
        let path = inner.path_of(&key);
        let uid = inner.uid;
        let at = path.clone();
        match blocking(move || inspect(&at, &key, uid)).await {
            Found::Good(nlink) => {
                bump(&counts.verify_hashes);
                let now = inner.now();
                let mut state = inner.state();
                state.index.touch(&key, now, true);
                state.index.insert(key, now, true);
                let source = Source::Entry { path, nlink };
                // A file the index did not list is a miss, adopted: nothing fetched.
                return Ok(Got {
                    source,
                    hit: indexed,
                });
            }
            Found::Bad => {
                if indexed {
                    bump(&inner.counters.verify_failures);
                    tracing::warn!("cache: entry {} failed its check; evicting it", key.name());
                }
                inner.state().index.remove(&key);
                let at = path.clone();
                let _ = blocking(move || remove_if_present(&at)).await;
            }
            Found::Absent => {
                inner.state().index.remove(&key);
            }
        }
        self.fill(cas, key, counts)
            .await
            .map(|source| Got { source, hit: false })
    }

    /// Writes `key`'s bytes into a new file in `tmp/` and inserts it; see the module
    /// docs. Fails only when the blob cannot be had (the CAS's error).
    async fn fill(
        &self,
        cas: &dyn Fetch,
        key: Key,
        counts: &InputCounts,
    ) -> Result<Source, CasError> {
        let inner = &*self.inner;
        if inner.state().index.get(&key.sibling()).is_some() {
            let (sibling, tmp) = (inner.path_of(&key.sibling()), inner.next_tmp());
            let at = tmp.clone();
            let copied = blocking(move || copy_checked(&sibling, &at, &key)).await;
            if let Some(file) = copied {
                return Ok(self.insert(key, file, tmp).await);
            }
        }
        let tmp = inner.next_tmp();
        let at = tmp.clone();
        let Ok(file) = blocking(move || new_file(&at, 0o600)).await else {
            tracing::warn!("cache: {} not inserted: no file in tmp/", key.name());
            return Ok(Source::Fetch);
        };
        // The CAS bounds the fetch (the transfer engine: its requests and its byte
        // budget for the whole process).
        let mut file = tokio::fs::File::from_std(file);
        if let Err(e) = cas.fetch_to(&key.digest(), &mut file).await {
            drop(file);
            let _ = blocking(move || remove_if_present(&tmp)).await;
            let CasError::Write(_, why) = e else {
                return Err(e);
            };
            tracing::warn!("cache: {} not inserted: {why}", key.name());
            return Ok(Source::Fetch);
        }
        counts.fetched_bytes.fetch_add(key.size, Ordering::Relaxed);
        let file = file.into_std().await;
        Ok(self.insert(key, file, tmp).await)
    }

    /// [`Inner::insert`] on the blocking pool.
    async fn insert(&self, key: Key, file: std::fs::File, tmp: PathBuf) -> Source {
        let owner = Arc::clone(&self.inner);
        blocking(move || owner.insert(key, file, tmp)).await
    }
}

/// Creates `path` (`O_EXCL`) with `mode`, for writing.
pub(super) fn new_file(path: &std::path::Path, mode: u32) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

/// Copies the entry at `from` into a new file at `to`, a MiB at a time, hashing as it
/// goes: the file, if the bytes are `key`'s. Otherwise (they are not, or a read or
/// write failed) `to` is removed.
fn copy_checked(from: &std::path::Path, to: &std::path::Path, key: &Key) -> Option<std::fs::File> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    let mut copied = 0u64;
    let mut file = new_file(to, 0o600).ok()?;
    let read = std::fs::File::open(from).and_then(|mut source| {
        let mut buffer = vec![0; 1 << 20];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                return Ok(());
            }
            hasher.update(&buffer[..n]);
            copied += n as u64;
            file.write_all(&buffer[..n])?;
        }
    });
    let hash: [u8; 32] = hasher.finalize().into();
    if read.is_ok() && copied == key.size && hash == key.hash {
        return Some(file);
    }
    drop(file);
    let _ = remove_if_present(to);
    None
}

impl Inner {
    /// The next step for `key`: its slot is waited on whoever holds it, an indexed and
    /// verified entry is a hit (unless `force`), and anything else takes the slot.
    fn step(&self, key: Key, force: bool) -> Step<'_> {
        let now = self.now();
        let mut state = self.state();
        if let Some(slot) = state.slots.get(&key) {
            return Step::Wait(slot.done.clone());
        }
        if !force && state.index.get(&key).is_some_and(|entry| entry.verified) {
            state.index.touch(&key, now, false);
            return Step::Hit;
        }
        Step::Own(self.take(&mut state, key))
    }

    /// Inserts the bytes written into `file` (named `tmp`) as `key`'s entry (see the
    /// module docs), then evicts down to the budget. On any failure the file is left
    /// in `tmp/` for the lease to copy.
    fn insert(&self, key: Key, file: std::fs::File, tmp: PathBuf) -> Source {
        let path = self.path_of(&key);
        match self.finish_entry(&key, file, &tmp, &path) {
            Ok(()) => {
                let now = self.now();
                self.state().index.insert(key, now, true);
                self.evict();
                Source::Entry { path, nlink: 1 }
            }
            Err(e) => {
                if e.kind() == ErrorKind::AlreadyExists {
                    tracing::warn!("cache: {} appeared during its fill", key.name());
                    self.state().unindexed.insert(key);
                } else {
                    tracing::warn!("cache: {} not inserted: {e}", key.name());
                    self.evict();
                }
                Source::Loose(tmp)
            }
        }
    }

    fn finish_entry(
        &self,
        key: &Key,
        file: std::fs::File,
        tmp: &PathBuf,
        path: &PathBuf,
    ) -> std::io::Result<()> {
        rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(key.mode()))?;
        let fixed = Timespec {
            tv_sec: FIXED_MTIME,
            tv_nsec: 0,
        };
        let times = Timestamps {
            last_access: fixed,
            last_modification: fixed,
        };
        rustix::fs::futimens(&file, &times)?;
        // Closed before the rename: a write descriptor open on an executable makes its
        // exec fail with ETXTBSY.
        drop(file);
        self.config.hooks.at(Point::Filled(key));
        rustix::fs::renameat_with(CWD, tmp, CWD, path, RenameFlags::NOREPLACE)?;
        Ok(())
    }
}

/// Runs `f` on the blocking pool and returns what it returns; a panic in `f` is
/// resumed here.
pub(super) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(resume)
}

/// Resumes the panic a blocking task ended with. The runtime is never shut down under
/// a lease, so a task that did not finish panicked.
fn resume<T>(error: tokio::task::JoinError) -> T {
    std::panic::resume_unwind(error.into_panic())
}
