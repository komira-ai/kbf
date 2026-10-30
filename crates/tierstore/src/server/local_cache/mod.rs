//! The local cache of a build node: a read-through disk cache in front of one upstream.
//!
//! A [`LocalCache`] is a [`Backend`] over an upstream [`Backend`] (an S3 endpoint, in the
//! binary: a storage node's tierstore). By default the S3 front serves it as a read cache and
//! refuses every write before it gets here (see `ServerConfig::cache`), so a cache has no
//! journal, no lease and no reconciler, and nothing to recover but a directory of files it can
//! delete. The only things that run on their own are the fills a miss starts and the eviction
//! pass ([`LocalCache::maintain`]).
//!
//! # Writes
//!
//! With [`CacheConfig::write_through`] (`write_through = true` in `[local_cache]`) the cache
//! accepts writes and sends every one of them to the upstream: nothing is acknowledged before
//! the upstream has it, and nothing is only local. The module `write` has what each call does.
//! Without the setting every write call of the [`Backend`] is `Unsupported`, as the front
//! refuses it before.
//!
//! # What is kept
//!
//! A key is kept if the longest prefix of the policies that holds it says `cache = true` and it
//! is not under `_journal/` or `_tiering/`. Every other key, and every listing, goes to the
//! upstream as it is and is never kept.
//!
//! - A hit (HEAD, GET, ranged GET of an object that is in the cache) is answered from the disk
//!   and makes no call to the upstream.
//! - A miss on a whole GET fills the object. A miss on a ranged GET fills the whole object if it
//!   is at most 64 MiB, and otherwise passes the range through and keeps nothing. A HEAD of an
//!   object that is not kept goes to the upstream and fills nothing.
//! - The misses of one key fill it once: the first starts the fill on a task of its own, the
//!   others wait for it. A fill that no reader waits for any more still ends.
//!
//! # A fill
//!
//! The object is read from the upstream in chunks (ranged reads of `fill_chunk` bytes, so that a
//! fill holds one chunk in memory whatever the object's size), each chunk written to a file in
//! `<path>/.stage/` and hashed as it passes. The file is kept only if it is whole: the upstream
//! gave every byte of the size it said, and a key named by its content (`cas/l/<sha256>`) has
//! bytes that hash to the name. Then the sidecar is renamed into place and the file is linked
//! into `obj/` with a create-only link, so a name appears whole or not at all (layout:
//! [`layout`]). A fill that fails is counted by reason and removed, and the reader that started it
//! is answered from the upstream as if the key were not kept (an upstream that failed is the
//! reader's error). The front holds a response whole, as it always has: the memory of a read is
//! the size of the range asked, never more because of a fill.
//!
//! A fill is skipped, and counted, when the object is larger than the budget or the cache, with the
//! bytes of the fills that are running, would pass its critical mark; the reader is served from
//! the upstream. A key that is not named by its content is read twice by HEAD, before the first
//! chunk and after the last: if the size or the tag changed, the object was written over during
//! the fill and is not kept. A key named by its content whose bytes do not hash to the name is
//! the reader's error, not passed through. A hit is not hashed again.
//!
//! At most `max_concurrent_fills` fills run at once, each holding one chunk (`fill_chunk`, 8 MiB)
//! in memory at a time, so the fills hold at most that many chunks. A miss of a key that has no
//! fill running, when every permit is taken, does not wait for one: it is served from the upstream
//! as a pass-through, keeps nothing and is counted in `fills_skipped` and `fills_skipped_busy`.
//! The readers of a key that has a fill running share it and need no permit. A fill that panics
//! or is cancelled frees its key and its permit, and the readers that waited for it get an error
//! (`Unavailable`, counted as a failed fill with the reason `aborted`).
//!
//! # Eviction
//!
//! The cache keeps its objects under `budget` bytes. Above `high_water` percent of it, and
//! whenever the file system's free space is under `free_floor` percent, a pass
//! ([`LocalCache::evict`]) removes the least recently used objects until the cache is down to
//! `low_water` percent and the free space is back at the floor. The order of use is in memory,
//! seeded at start from the files' times and written back to them (`utimensat`, once per object
//! per pass however often it was used) so that a restart keeps it. An object with more than one
//! link is in use by another tree: it is skipped, counted, and stays charged to the budget.
//! Files in `.stage/` are never evicted and are removed at start.

mod disk;
mod evict;
mod fill;
mod index;
pub(super) mod layout;
mod listing;
mod metrics;
mod selfcheck;
mod write;

#[cfg(test)]
mod evict_tests;
#[cfg(test)]
mod fill_tests;
#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod hit_tests;
#[cfg(test)]
mod limits_tests;
#[cfg(test)]
mod listing_tests;
#[cfg(test)]
mod selfcheck_tests;
#[cfg(test)]
mod start_tests;
#[cfg(test)]
pub(in crate::server) mod testing;
#[cfg(test)]
mod write_front_tests;
#[cfg(test)]
mod write_race_tests;
#[cfg(test)]
mod write_tests;

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Semaphore;

use crate::journal::is_reserved;
use crate::{Backend, ByteRange, Clock, Error, Listed, Meta, OpenUpload, PartBody, UploadsAfter};

pub use disk::{Disk, Space, SystemDisk};
pub use metrics::Counters;
pub use selfcheck::{LocalTierFailure, check_local};

use fill::Pending;
use index::Index;
use layout::{Name, data_path, name_of};
use metrics::{add, bump};

/// The most an uncached object may be, for a ranged read of it to fill the whole of it.
pub const WHOLE_FILL_LIMIT: u64 = 64 << 20;

/// The bytes a fill reads from the upstream at a time.
pub const FILL_CHUNK: u64 = 8 << 20;

/// The fills that may run at once, unless the settings say otherwise.
pub const MAX_CONCURRENT_FILLS: usize = 4;

/// The text of the refusal of a write at the Backend level (the front refuses it earlier).
const READ_ONLY: &str = "this tierstore is a read cache";

/// The settings of a [`LocalCache`].
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// The directory of the cache: made if it is not there.
    pub path: PathBuf,
    /// The bytes the cache is meant to hold.
    pub budget: u64,
    /// The percent of the budget above which a pass evicts.
    pub high_water: u8,
    /// The percent of the budget a pass evicts down to.
    pub low_water: u8,
    /// The percent of the budget above which a fill is skipped.
    pub critical: u8,
    /// The percent of the file system that is kept free: below it a pass evicts.
    pub free_floor: u8,
    /// The prefixes and whether the keys under each are kept; the longest prefix that holds a
    /// key decides, and a key under none is not kept.
    pub policies: Vec<(String, bool)>,
    /// The bytes a fill reads at a time.
    pub fill_chunk: u64,
    /// The most an object may be for a ranged read of it to fill it.
    pub whole_limit: u64,
    /// The most fills that run at once. A miss that finds all of them running is served from
    /// the upstream and keeps nothing. At least one (0 is read as 1).
    pub max_concurrent_fills: usize,
    /// Whether the cache takes writes and sends them to the upstream. Off, every write call is
    /// `Unsupported`. The binary sets the front's `cache_writes` from the same setting, so that
    /// the front does not refuse the writes either.
    pub write_through: bool,
}

impl CacheConfig {
    /// A cache in `path` of `budget` bytes, with the water marks at 90, 75 and 95 percent, a
    /// floor of 10 percent free, the fill sizes of the binary, and no writes.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, budget: u64, policies: Vec<(String, bool)>) -> Self {
        Self {
            path: path.into(),
            budget,
            high_water: 90,
            low_water: 75,
            critical: 95,
            free_floor: 10,
            policies,
            fill_chunk: FILL_CHUNK,
            whole_limit: WHOLE_FILL_LIMIT,
            max_concurrent_fills: MAX_CONCURRENT_FILLS,
            write_through: false,
        }
    }

    /// The most the budget may be on a file system of `total` bytes: its size less the
    /// `free_floor` percent that is kept free.
    #[must_use]
    pub fn room(&self, total: u64) -> u64 {
        total.saturating_sub(percent_of(total, self.free_floor))
    }
}

/// `percent` percent of `total`.
fn percent_of(total: u64, percent: u8) -> u64 {
    let share = u128::from(total) * u128::from(percent) / 100;
    u64::try_from(share).unwrap_or(u64::MAX)
}

/// The mutex's contents, whether or not a thread panicked holding it: the state it guards is
/// kept consistent by each step.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The offset and length that `range` asks of an object of `size` bytes.
fn window(range: Option<ByteRange>, size: u64) -> Result<(u64, u64), Error> {
    let Some(range) = range else {
        return Ok((0, size));
    };
    if range.offset() > size {
        let text = format!(
            "range starting at {} is past the end of the {size}-byte object",
            range.offset()
        );
        return Err(Error::Other(text));
    }
    Ok((range.offset(), range.size().min(size - range.offset())))
}

/// `len` bytes of the file at `path` from `offset`.
fn read_at(path: &Path, offset: u64, len: u64) -> io::Result<Bytes> {
    let file = std::fs::File::open(path)?;
    // `len` is at most the length of the file, which a 64-bit address space holds.
    let mut bytes = vec![0; len as usize];
    file.read_exact_at(&mut bytes, offset)?;
    Ok(Bytes::from(bytes))
}

/// Whether `error` says the file is not there, or not as long as the cache believes.
fn gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::UnexpectedEof
    )
}

/// What the cache is made of; a [`LocalCache`] is a handle to one, so a fill that runs on its
/// own task keeps it alive.
pub(super) struct Inner {
    upstream: Arc<dyn Backend>,
    disk: Arc<dyn Disk>,
    clock: Arc<dyn Clock>,
    config: CacheConfig,
    index: Mutex<Index>,
    counters: Counters,
    /// The fills that are running, by the name of the object.
    flights: Mutex<HashMap<String, Pending>>,
    /// The fills that are running for an object that was deleted or replaced since they began:
    /// they must not keep what they read. A name is put here only while it has a fill in
    /// `flights`, and leaves with it; the lock is taken with `flights` held.
    invalidated: Mutex<HashSet<String>>,
    /// The number of the next file in `.stage/`.
    staged: AtomicU64,
    /// The bytes of the fills that are running and have no object in the index yet.
    reserved: AtomicU64,
    /// One permit for each fill that may run; a fill holds one until it ends.
    permits: Arc<Semaphore>,
    /// Whether a pass of the eviction is running.
    evicting: AtomicBool,
}

/// A read-through cache of an upstream, on a directory of the local file system.
#[derive(Clone)]
pub struct LocalCache {
    inner: Arc<Inner>,
}

impl LocalCache {
    /// Opens the cache in `config.path`: makes the directory, removes what a fill that was
    /// running left in `.stage/`, and reads the objects back (an object whose sidecar does not
    /// say what the file holds is deleted), the least recently used first by the time of the
    /// file.
    ///
    /// # Errors
    /// What the system said about the directory.
    pub fn open(
        config: CacheConfig,
        upstream: Arc<dyn Backend>,
        disk: Arc<dyn Disk>,
        clock: Arc<dyn Clock>,
    ) -> io::Result<Self> {
        let removed = layout::clear_stage(&config.path)?;
        let mut scanned = layout::scan(&config.path)?;
        scanned
            .found
            .sort_by(|a, b| (a.used, &a.hex).cmp(&(b.used, &b.hex)));
        let mut index = Index::default();
        for found in scanned.found {
            let sidecar = found.sidecar;
            index.insert(&found.hex, sidecar.size, sidecar.tag, sidecar.modified);
        }
        let counters = Counters::default();
        add(&counters.discarded_at_start, scanned.discarded + removed);
        let permits = Arc::new(Semaphore::new(config.max_concurrent_fills.max(1)));
        let inner = Inner {
            upstream,
            disk,
            clock,
            config,
            index: Mutex::new(index),
            counters,
            flights: Mutex::new(HashMap::new()),
            invalidated: Mutex::new(HashSet::new()),
            staged: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            permits,
            evicting: AtomicBool::new(false),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The directory of the cache.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.config.path
    }

    /// The bytes and the number of the objects in the cache.
    #[must_use]
    pub fn held(&self) -> (u64, usize) {
        let index = locked(&self.inner.index);
        (index.bytes(), index.len())
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.inner.counters
    }

    /// The text of `/metricz`: the counters, the bytes in the cache and the free space of the
    /// file system now.
    #[must_use]
    pub fn metrics(&self) -> String {
        let inner = &self.inner;
        let (bytes, objects) = self.held();
        let free = inner
            .disk
            .space(&inner.config.path)
            .map_or(0, |space| space.free);
        inner.counters.render(bytes, objects as u64, free)
    }
}

impl Inner {
    /// The on-disk name of `key` if the cache keeps it.
    fn cacheable(&self, key: &str) -> Option<Name> {
        if is_reserved(key) {
            return None;
        }
        let policies = &self.config.policies;
        let holding = policies
            .iter()
            .filter(|(prefix, _)| key.starts_with(prefix.as_str()));
        let longest = holding.max_by_key(|(prefix, _)| prefix.len());
        longest.is_some_and(|(_, keep)| *keep).then(|| name_of(key))
    }

    /// What the cache says of the object, if it holds it.
    fn kept(&self, name: &Name) -> Option<Meta> {
        locked(&self.index).get(&name.hex).map(index::Entry::meta)
    }

    /// Removes the object from the index and the disk.
    fn drop_entry(&self, hex: &str) {
        drop(locked(&self.index).remove(hex));
        let root = &self.config.path;
        let _ = std::fs::remove_file(data_path(root, hex));
        let _ = std::fs::remove_file(layout::meta_path(root, hex));
    }

    /// The bytes of `range` of the object if it is in the cache: `hit` says whether this is a
    /// read the cache answers on its own (a hit) or the read that follows a fill.
    async fn lookup(
        &self,
        name: &Name,
        range: Option<ByteRange>,
        hit: bool,
    ) -> Result<Option<Bytes>, Error> {
        let used = locked(&self.index).used(&name.hex, self.clock.now());
        let Some(size) = used else {
            return Ok(None);
        };
        let (offset, len) = window(range, size)?;
        let path = data_path(&self.config.path, &name.hex);
        let reading = tokio::task::spawn_blocking(move || read_at(&path, offset, len));
        match reading.await.expect("a read does not panic") {
            Ok(bytes) => {
                let counters = &self.counters;
                if hit {
                    bump(&counters.hits);
                    add(&counters.bytes_served_local, len);
                } else {
                    add(&counters.bytes_served_upstream, len);
                }
                Ok(Some(bytes))
            }
            Err(error) if gone(&error) => {
                // The file is gone or short: the entry is a lie, so it is dropped.
                self.drop_entry(&name.hex);
                Ok(None)
            }
            // Out of descriptors, a failing disk: the object may be fine, so it stays.
            Err(_) => Ok(None),
        }
    }

    /// The read of `range` of `key` from the upstream, which keeps nothing.
    async fn passthrough(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let bytes = self.upstream.get(key, range).await?;
        add(&self.counters.bytes_served_upstream, bytes.len() as u64);
        Ok(bytes)
    }
}

#[async_trait]
impl Backend for LocalCache {
    /// Whether a streamed body must come with its length: the upstream's answer once the cache
    /// takes writes, and `false` for a read cache, which takes no body.
    fn needs_length(&self) -> bool {
        let inner = &self.inner;
        inner.config.write_through && inner.upstream.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put_whole(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_through(key, len, body).await
    }

    async fn put_if_match(&self, _key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Unsupported(READ_ONLY.to_owned()))
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete_through(key).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.inner.create_through(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sent = self.inner.part_through(key, id, number, None, body);
        sent.await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sent = self.inner.part_through(key, id, number, len, body);
        sent.await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_through(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.inner.abort_through(key, id).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let inner = &self.inner;
        let Some(name) = inner.cacheable(key) else {
            return inner.passthrough(key, range).await;
        };
        if let Some(bytes) = inner.lookup(&name, range, true).await? {
            return Ok(bytes);
        }
        if let Some(wanted) = range {
            let meta = inner.upstream.head(key).await?;
            if meta.size > inner.config.whole_limit {
                bump(&inner.counters.misses);
                return inner.passthrough(key, Some(wanted)).await;
            }
        }
        bump(&inner.counters.misses);
        if inner.fill_once(key, &name).await? == fill::Filled::Kept
            && let Some(bytes) = inner.lookup(&name, range, false).await?
        {
            return Ok(bytes);
        }
        inner.passthrough(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let inner = &self.inner;
        let kept = inner.cacheable(key).and_then(|name| inner.kept(&name));
        match kept {
            Some(meta) => Ok(meta),
            None => inner.upstream.head(key).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list_data(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let page = self.inner.list_data_page(prefix, start_after, max_keys);
        page.await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let found = self.inner.list_data_uploads(prefix, after, max_uploads);
        found.await
    }
}
