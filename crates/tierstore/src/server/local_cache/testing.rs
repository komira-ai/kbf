//! Fixtures of the tests of the local cache: an upstream whose answers a test can cut short or
//! hold, a file system whose answers a test can change, and a cache over them in a scratch
//! directory.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{
    Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op, OpenUpload,
    PartBody, TestClock, UploadsAfter,
};

use super::layout::{data_path, name_of};
use super::{CacheConfig, Disk, LocalCache, Space, SystemDisk};

/// `len` bytes that differ from one place to the next, so that a wrong range shows.
pub(in crate::server) fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) % 251) as u8).collect()
}

/// The hex of the SHA-256 of `bytes`: the name of a blob under `cas/l/`.
pub(in crate::server) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// An upstream that holds objects in memory and can be told to give short bodies, to hold a
/// read until the test lets it go, and to fail or panic in the next read it lets go.
pub(in crate::server) struct Tamper {
    pub(in crate::server) memory: MemoryBackend,
    pub(in crate::server) short: AtomicBool,
    /// The next read, once the gate lets it go, is an `Unavailable` error.
    pub(in crate::server) fail_once: AtomicBool,
    /// The next read, once the gate lets it go, panics.
    pub(in crate::server) panic_once: AtomicBool,
    /// The gate and the one key it holds (every key if none).
    gate: Mutex<Option<Gate>>,
    /// The gate that holds a read after it has read the object, and the one key it holds.
    after: Mutex<Option<Gate>>,
    /// The reads that have read their object and wait at `after`, counted when they arrive.
    parked: AtomicU64,
    /// The gate that holds a delete before it deletes.
    deletes: Mutex<Option<Arc<Semaphore>>>,
    /// The deletes that wait at `deletes`, counted when they arrive.
    deleting: AtomicU64,
    /// What `needs_length` says.
    pub(in crate::server) needs_len: AtomicBool,
}

/// A semaphore that reads wait on, and the key whose reads wait (every key if none).
type Gate = (Option<String>, Arc<Semaphore>);

impl Tamper {
    fn new() -> Self {
        Self {
            memory: MemoryBackend::new(),
            short: AtomicBool::new(false),
            fail_once: AtomicBool::new(false),
            panic_once: AtomicBool::new(false),
            gate: Mutex::new(None),
            after: Mutex::new(None),
            parked: AtomicU64::new(0),
            deletes: Mutex::new(None),
            deleting: AtomicU64::new(0),
            needs_len: AtomicBool::new(false),
        }
    }

    /// Holds every delete from now on, before it deletes, until a permit is added to the gate
    /// that is returned.
    pub(in crate::server) fn hold_deletes(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        let mut held = self.deletes.lock().unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::clone(&gate));
        gate
    }

    /// How many deletes have arrived at the gate of [`hold_deletes`](Self::hold_deletes).
    pub(in crate::server) fn deleting(&self) -> u64 {
        self.deleting.load(Ordering::SeqCst)
    }

    /// Holds the reads of `key` from now on after they have read the object, until a permit is
    /// added to the gate that is returned: the bytes are in hand, and the answer is not given.
    pub(in crate::server) fn hold_key_after_read(&self, key: &str) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        let mut held = self.after.lock().unwrap_or_else(PoisonError::into_inner);
        *held = Some((Some(key.to_owned()), Arc::clone(&gate)));
        gate
    }

    /// How many reads have read their object and wait at the gate of
    /// [`hold_key_after_read`](Self::hold_key_after_read).
    pub(in crate::server) fn parked(&self) -> u64 {
        self.parked.load(Ordering::SeqCst)
    }

    /// Holds every read from now on until a permit is added to the gate that is returned.
    pub(in crate::server) fn hold(&self) -> Arc<Semaphore> {
        self.gated(None)
    }

    /// Holds the reads of `key` only, from now on, until a permit is added to the gate that is
    /// returned.
    pub(in crate::server) fn hold_key(&self, key: &str) -> Arc<Semaphore> {
        self.gated(Some(key.to_owned()))
    }

    fn gated(&self, key: Option<String>) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        let mut held = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        *held = Some((key, Arc::clone(&gate)));
        gate
    }
}

#[async_trait]
impl Backend for Tamper {
    fn needs_length(&self) -> bool {
        self.needs_len.load(Ordering::SeqCst)
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.memory.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.memory.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some((_, gate)) = gate.filter(|(only, _)| only.as_deref().is_none_or(|k| k == key)) {
            gate.acquire().await.expect("the gate stays open").forget();
        }
        assert!(
            !self.panic_once.swap(false, Ordering::SeqCst),
            "the upstream was told to panic"
        );
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(Error::Unavailable(
                "the upstream was told to fail".to_owned(),
            ));
        }
        let body = self.memory.get(key, range).await?;
        let after = self
            .after
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some((_, gate)) = after.filter(|(only, _)| only.as_deref().is_none_or(|k| k == key))
        {
            let _ = self.parked.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.expect("the gate stays open").forget();
        }
        if self.short.load(Ordering::SeqCst) {
            return Ok(body.slice(..body.len().saturating_sub(1)));
        }
        Ok(body)
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.memory.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        let gate = self
            .deletes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(gate) = gate {
            let _ = self.deleting.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.expect("the gate stays open").forget();
        }
        self.memory.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.memory.list(prefix).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.memory.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.memory.upload_part(key, id, number, body).await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.memory.complete_upload(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        self.memory.abort_upload(key, id).await
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        self.memory.list_uploads(prefix, after, max_uploads).await
    }
}

/// What a [`FakeDisk`] does wrongly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum Quirk {
    /// Nothing: it is the file system of the machine.
    None,
    /// A second link to a name that exists succeeds and changes nothing.
    LinkOverwrites,
    /// The name made by a link holds other bytes than the file it was linked to.
    SecondDiffers,
    /// The file that was linked holds other bytes after the link.
    FirstDiffers,
    /// Every file reports one link.
    OneLink,
    /// Every link fails with this kind of error.
    LinkFails(io::ErrorKind),
    /// `space` fails, as a `statvfs` that the system refuses.
    SpaceFails,
}

/// The file system of the machine, with the answers a test changes.
pub(in crate::server) struct FakeDisk {
    pub(in crate::server) quirk: Mutex<Quirk>,
    /// The space `space` reports, if a test sets one.
    pub(in crate::server) space: Mutex<Option<Space>>,
    /// Every call of `touch`, in order.
    pub(in crate::server) touched: Mutex<Vec<(PathBuf, u64)>>,
    /// The number of calls of `link`.
    links_made: Mutex<u32>,
    /// The names of every call of `link`, in order: from and to.
    pub(in crate::server) linked: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl FakeDisk {
    pub(in crate::server) fn new() -> Self {
        Self {
            quirk: Mutex::new(Quirk::None),
            space: Mutex::new(None),
            touched: Mutex::new(Vec::new()),
            links_made: Mutex::new(0),
            linked: Mutex::new(Vec::new()),
        }
    }

    pub(in crate::server) fn set(&self, quirk: Quirk) {
        *self.quirk.lock().unwrap_or_else(PoisonError::into_inner) = quirk;
    }

    pub(in crate::server) fn set_space(&self, total: u64, free: u64) {
        let space = Space { total, free };
        *self.space.lock().unwrap_or_else(PoisonError::into_inner) = Some(space);
    }

    fn quirk(&self) -> Quirk {
        *self.quirk.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Disk for FakeDisk {
    fn space(&self, path: &Path) -> io::Result<Space> {
        if self.quirk() == Quirk::SpaceFails {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let set = *self.space.lock().unwrap_or_else(PoisonError::into_inner);
        match set {
            Some(space) => Ok(space),
            None => SystemDisk.space(path),
        }
    }

    fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
        let call = {
            let mut made = self
                .links_made
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *made += 1;
            *made
        };
        self.linked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((from.to_path_buf(), to.to_path_buf()));
        let quirk = self.quirk();
        if let Quirk::LinkFails(kind) = quirk {
            return Err(io::Error::from(kind));
        }
        if quirk == Quirk::LinkOverwrites && call == 2 {
            return Ok(());
        }
        SystemDisk.link(from, to)?;
        match quirk {
            Quirk::SecondDiffers => {
                std::fs::remove_file(to)?;
                std::fs::write(to, b"other bytes")?;
            }
            Quirk::FirstDiffers => {
                std::fs::remove_file(from)?;
                std::fs::write(from, b"other bytes")?;
            }
            _ => {}
        }
        Ok(())
    }

    fn links(&self, path: &Path) -> io::Result<u64> {
        match self.quirk() {
            Quirk::OneLink => Ok(1),
            _ => SystemDisk.links(path),
        }
    }

    fn touch(&self, path: &Path, millis: u64) -> io::Result<()> {
        self.touched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((path.to_path_buf(), millis));
        SystemDisk.touch(path, millis)
    }
}

/// A cache over a [`Tamper`] upstream in a scratch directory.
pub(in crate::server) struct Rig {
    pub(in crate::server) dir: TestDir,
    pub(in crate::server) tamper: Arc<Tamper>,
    pub(in crate::server) upstream: Arc<FaultyBackend>,
    pub(in crate::server) clock: Arc<TestClock>,
    pub(in crate::server) disk: Arc<FakeDisk>,
    pub(in crate::server) config: CacheConfig,
    pub(in crate::server) cache: LocalCache,
}

impl Rig {
    /// A cache of a mebibyte that keeps what is under `cas/`, in a scratch directory called
    /// `name`.
    pub(in crate::server) fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// The same, with `write_through` on.
    pub(in crate::server) fn writing(name: &str) -> Self {
        Self::with(name, |config| config.write_through = true)
    }

    /// The files in the stage directory of the cache now.
    pub(in crate::server) fn staged_files(&self) -> usize {
        std::fs::read_dir(self.config.path.join(".stage"))
            .expect("the stage directory")
            .count()
    }

    /// The same, with the settings changed by `change`.
    pub(in crate::server) fn with(name: &str, change: impl FnOnce(&mut CacheConfig)) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let path = dir.path().join("cache");
        let mut config = CacheConfig::new(path, 1 << 20, vec![("cas/".to_owned(), true)]);
        change(&mut config);
        let tamper = Arc::new(Tamper::new());
        let upstream = Arc::new(FaultyBackend::new(tamper.clone()));
        let clock = Arc::new(TestClock::new());
        let disk = Arc::new(FakeDisk::new());
        let cache = Self::open(&config, &upstream, &disk, &clock);
        Self {
            dir,
            tamper,
            upstream,
            clock,
            disk,
            config,
            cache,
        }
    }

    fn open(
        config: &CacheConfig,
        upstream: &Arc<FaultyBackend>,
        disk: &Arc<FakeDisk>,
        clock: &Arc<TestClock>,
    ) -> LocalCache {
        let upstream: Arc<dyn Backend> = upstream.clone();
        let (disk, clock) = (disk.clone(), clock.clone());
        LocalCache::open(config.clone(), upstream, disk, clock).expect("a cache")
    }

    /// The cache again over the same directory, as after a restart.
    pub(in crate::server) fn reopen(&self) -> LocalCache {
        self.reopen_with(|_| {})
    }

    /// The same, with the settings changed by `change`.
    pub(in crate::server) fn reopen_with(
        &self,
        change: impl FnOnce(&mut CacheConfig),
    ) -> LocalCache {
        let mut config = self.config.clone();
        change(&mut config);
        Self::open(&config, &self.upstream, &self.disk, &self.clock)
    }

    /// Puts an object in the upstream.
    pub(in crate::server) async fn put(&self, key: &str, body: &[u8]) {
        let body = Bytes::copy_from_slice(body);
        self.tamper.put(key, body).await.expect("a put");
    }

    /// A whole read through the cache.
    pub(in crate::server) async fn get(&self, key: &str) -> Result<Bytes, Error> {
        self.cache.get(key, None).await
    }

    /// The calls of a kind the upstream has had.
    pub(in crate::server) fn calls(&self, op: Op) -> u64 {
        self.upstream.calls(op)
    }

    /// The data file of `key` in the cache directory.
    pub(in crate::server) fn file(&self, key: &str) -> PathBuf {
        data_path(&self.config.path, &name_of(key).hex)
    }

    /// Whether the cache directory has a data file for `key`.
    pub(in crate::server) fn has_file(&self, key: &str) -> bool {
        self.file(key).is_file()
    }
}
