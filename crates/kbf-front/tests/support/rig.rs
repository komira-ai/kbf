//! The write pool's test rig, shared by the test binaries that drive a `Cache` over a
//! counting filesystem: a [`Rig`] (one metadata log that outlives a restart, a faulty
//! store, and a tier directory on a `FaultFs` wrapped in a [`Counting`] filesystem, the
//! tier's file-open seam), and the helpers that stream a blob into a pooled cache.
//!
//! [`Counting`] counts the files open for writing, their syncs and their writes, can pause
//! the syncs ([`Counting::pause`], [`Counting::wait_waiting`]) and can fail the writes,
//! creates or syncs. Included with `#[path]` after `mod common;` and the `write_tier`
//! support module, whose [`Log`] it runs over; each binary uses part of it.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use kbf_front::{Cache, CacheError, PoolConfig, StreamedRecord, TierStartup};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_store::{FaultFs, Fs, FsFile};
use kbf_tier::TierOptions;

use crate::common::faulty::FaultyStore;
use crate::support::{Log, SEGMENT, SERVER, TestBlob, TierCache};

const GIB: u64 = 1 << 30;

/// What the filesystem seam counts and does. Shared by every clone and every file.
#[derive(Debug, Default)]
pub struct Probe {
    /// Files opened for writing and not yet dropped, and the most there were at once.
    pub open: AtomicUsize,
    pub peak: AtomicUsize,
    /// File syncs that completed, and bytes written.
    pub syncs: AtomicUsize,
    pub written: AtomicU64,
    pub fail_writes: AtomicBool,
    pub fail_syncs: AtomicBool,
    pub fail_creates: AtomicBool,
    gate: Mutex<Gate>,
    changed: Condvar,
}

#[derive(Debug, Default)]
pub struct Gate {
    paused: bool,
    /// Syncs waiting for the pause to end.
    waiting: usize,
}

impl Probe {
    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A `FaultFs` that tells the [`Probe`] about its files.
#[derive(Clone, Debug)]
pub struct Counting {
    pub inner: FaultFs,
    pub probe: Arc<Probe>,
}

#[derive(Debug)]
pub struct CountedFile {
    inner: <FaultFs as Fs>::File,
    probe: Arc<Probe>,
    counted: bool,
}

impl Drop for CountedFile {
    fn drop(&mut self) {
        if self.counted {
            self.probe.open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl FsFile for CountedFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(bytes)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        if self.probe.fail_writes.load(Ordering::SeqCst) {
            return Err(io::Error::other("a write failed by the test"));
        }
        self.inner.write_at(offset, bytes)?;
        self.probe
            .written
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.inner.truncate(len)
    }

    fn sync(&self) -> io::Result<()> {
        {
            let mut gate = self.probe.gate();
            gate.waiting += 1;
            self.probe.changed.notify_all();
            while gate.paused {
                gate = self
                    .probe
                    .changed
                    .wait(gate)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            gate.waiting -= 1;
            self.probe.changed.notify_all();
        }
        if self.probe.fail_syncs.load(Ordering::SeqCst) {
            return Err(io::Error::other("a sync failed by the test"));
        }
        self.inner.sync()?;
        self.probe.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl Fs for Counting {
    type File = CountedFile;

    fn list(&self) -> io::Result<Vec<String>> {
        self.inner.list()
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        self.inner.read(name)
    }

    fn file_len(&self, name: &str) -> io::Result<u64> {
        self.inner.file_len(name)
    }

    fn read_at(&self, name: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_at(name, offset, len)
    }

    fn create(&self, name: &str) -> io::Result<CountedFile> {
        let inner = self.inner.create(name)?;
        Ok(self.uncounted(inner))
    }

    fn create_positioned(&self, name: &str) -> io::Result<CountedFile> {
        if self.probe.fail_creates.load(Ordering::SeqCst) {
            return Err(io::Error::other("a create failed by the test"));
        }
        let inner = self.inner.create_positioned(name)?;
        Ok(self.counted(inner))
    }

    fn open_append(&self, name: &str) -> io::Result<CountedFile> {
        let inner = self.inner.open_append(name)?;
        Ok(self.uncounted(inner))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.inner.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()
    }
}

impl Counting {
    pub fn new(inner: FaultFs) -> Self {
        Self {
            inner,
            probe: Arc::default(),
        }
    }

    /// A file that is not a block (the `TIER` marker): not counted.
    fn uncounted(&self, inner: <FaultFs as Fs>::File) -> CountedFile {
        CountedFile {
            inner,
            probe: Arc::clone(&self.probe),
            counted: false,
        }
    }

    /// A block file opened for positioned writes: counted until dropped.
    fn counted(&self, inner: <FaultFs as Fs>::File) -> CountedFile {
        let now = self.probe.open.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe.peak.fetch_max(now, Ordering::SeqCst);
        CountedFile {
            inner,
            probe: Arc::clone(&self.probe),
            counted: true,
        }
    }

    /// Syncs wait from now until [`Counting::resume`].
    pub fn pause(&self) {
        self.probe.gate().paused = true;
    }

    pub fn resume(&self) {
        self.probe.gate().paused = false;
        self.probe.changed.notify_all();
    }

    pub fn waiting(&self) -> usize {
        self.probe.gate().waiting
    }

    /// Blocks until `n` syncs are waiting at the pause: a positive event, not a poll. Call
    /// it from a thread that may block (`spawn_blocking`, or a test thread outside the
    /// runtime's workers); the syncs' own threads are the ones it waits for.
    pub fn wait_waiting(&self, n: usize) {
        let mut gate = self.probe.gate();
        while gate.waiting != n {
            gate = self
                .probe
                .changed
                .wait(gate)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub fn syncs(&self) -> usize {
        self.probe.syncs.load(Ordering::SeqCst)
    }

    pub fn peak(&self) -> usize {
        self.probe.peak.load(Ordering::SeqCst)
    }
}

/// One farm: a log and a store that outlive a restart, and a tier directory.
pub struct Rig {
    pub log: Log,
    pub store: FaultyStore,
    pub fs: Counting,
}

impl Drop for Rig {
    /// A test that fails while the syncs are paused must not leave the writers (and the
    /// runtime waiting for them) stuck.
    fn drop(&mut self) {
        self.fs.resume();
    }
}

impl Rig {
    pub fn new() -> Self {
        Self {
            log: Log::new(),
            store: FaultyStore::new(Capabilities::default()),
            fs: Counting::new(FaultFs::new()),
        }
    }

    /// A start over this rig's log and store on `fs`, with a tier of `options` and a
    /// write pool of `pool`.
    pub async fn start_on(
        &self,
        fs: &Counting,
        options: TierOptions,
        pool: PoolConfig,
    ) -> (Arc<TierCache>, TierStartup) {
        let cache = Cache::open(self.log.clone(), self.store.clone(), KeyPrefix::default())
            .await
            .expect("open")
            .with_segment_limit(SEGMENT);
        let (cache, startup) = pooled(cache, pool)
            .attach_tier(fs.clone(), options, SERVER, None)
            .await
            .expect("attach");
        // The start synced the `TIER` marker; the tests count the blobs' syncs.
        fs.probe.syncs.store(0, Ordering::SeqCst);
        (Arc::new(cache), startup)
    }

    pub async fn start(&self, writers: usize, queue: usize) -> Arc<TierCache> {
        let pool = pool(writers, queue);
        self.start_on(&self.fs, TierOptions::new(GIB), pool).await.0
    }

    /// Whether `digest` is staged on the tier (not in the store).
    pub fn on_tier(&self, digest: &kbf_types::Digest) -> bool {
        self.log.located(digest).1.is_some()
    }
}

pub fn pool(writers: usize, queue: usize) -> PoolConfig {
    PoolConfig {
        writers: writers.try_into().expect("writers"),
        queue: queue.try_into().expect("queue"),
        ..PoolConfig::DEFAULT
    }
}

pub fn pooled(cache: TierCache, config: PoolConfig) -> TierCache {
    cache.with_write_pool(config)
}

/// A blob streamed in 64 KiB pieces and ended as its digest: ready to commit.
pub async fn prepare(
    cache: &Arc<TierCache>,
    blob: &TestBlob,
) -> Result<StreamedRecord<Log, FaultyStore>, CacheError> {
    let (cache, blob) = (Arc::clone(cache), blob.clone());
    tokio::task::spawn_blocking(move || {
        let mut stream = cache.stream(blob.bytes.len() as u64).expect("a stream");
        for piece in blob.bytes.chunks(64 << 10) {
            stream.append(piece)?;
        }
        stream.finish(blob.digest, kbf_segments::crc32c(&blob.bytes))
    })
    .await
    .expect("join")
}

pub async fn put(cache: &Arc<TierCache>, blob: &TestBlob) -> Result<(), CacheError> {
    prepare(cache, blob).await?.commit().await
}
