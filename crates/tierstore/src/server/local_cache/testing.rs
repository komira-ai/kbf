//! Fixtures of the tests of the local cache: an upstream whose answers a test can cut short or
//! hold, a file system whose answers a test can change, and a cache over them in a scratch
//! directory.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{
    Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op, OpenUpload,
    PartBody, TestClock, UploadsAfter,
};

use super::layout::{data_path, name_of};
use super::{CacheConfig, Disk, LocalCache, Space, SystemDisk};

/// What a listing of a directory tree says of one entry.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::server) struct Seen {
    pub(in crate::server) dir: bool,
    pub(in crate::server) len: u64,
    pub(in crate::server) mode: u32,
    pub(in crate::server) inode: u64,
    /// The time of last change of the contents, in seconds and nanoseconds.
    pub(in crate::server) mtime: (i64, i64),
}

/// Every entry under `root` (and `root` itself, as the empty path) with its kind, size, mode,
/// inode and time of last change. Two listings are equal if nothing under `root` was made,
/// removed, rewritten, renamed, chmodded or stamped between them.
pub(in crate::server) fn tree(root: &Path) -> BTreeMap<PathBuf, Seen> {
    let seen_of = |path: &Path| {
        let meta = std::fs::symlink_metadata(path).expect("an entry of the tree");
        Seen {
            dir: meta.is_dir(),
            len: meta.len(),
            mode: meta.permissions().mode(),
            inode: meta.ino(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    };
    let mut listing = BTreeMap::new();
    listing.insert(PathBuf::new(), seen_of(root));
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("a directory of the tree") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            let below = path.strip_prefix(root).expect("under the root");
            listing.insert(below.to_path_buf(), seen_of(&path));
        }
    }
    listing
}

/// The wait and the deadline of a keep in tests that do not test them: an hour, so that no test
/// that runs on the real clock can reach either, whatever the load on the disk. The tests of the
/// two settings run on a paused clock and set small values themselves.
pub(in crate::server) fn never_expires(config: &mut CacheConfig) {
    config.keep_wait = std::time::Duration::from_secs(3600);
    config.keep_deadline = std::time::Duration::from_secs(3600);
}

/// The turns of the scheduler a test gives an event that another task or a thread brings about
/// before it fails: some seconds of real time, which a disk under load needs for a file sync, and
/// no virtual time, which only the test moves.
pub(in crate::server) const TURNS: u32 = 10_000_000;

/// Waits, by giving up the thread, until `done` is true; fails the test, saying what `what` was
/// to have happened, if it has not after [`TURNS`] turns. A wait that is not bounded turns a
/// regression into a test that runs until it is killed.
pub(in crate::server) async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..TURNS {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("after {TURNS} turns of the scheduler, this never happened: {what}");
}

/// [`wait_until`] on a paused clock, which it moves by `step` at each turn: for an event that
/// waits for a timer (a retry, a deadline) as well as for another task.
pub(in crate::server) async fn wait_advancing(
    what: &str,
    step: std::time::Duration,
    mut done: impl FnMut() -> bool,
) {
    for _ in 0..TURNS {
        if done() {
            return;
        }
        tokio::time::advance(step).await;
        tokio::task::yield_now().await;
    }
    panic!("after {TURNS} turns of the scheduler, this never happened: {what}");
}

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
    /// Every read gives one byte more than it was asked for.
    pub(in crate::server) long: AtomicBool,
    /// Every HEAD gives an empty content tag.
    pub(in crate::server) blank_tags: AtomicBool,
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
    /// The next delete is applied and then answers with an `Unavailable` error, as a store does
    /// whose answer is lost after it committed.
    pub(in crate::server) lose_delete_answer: AtomicBool,
    /// What `needs_length` says.
    pub(in crate::server) needs_len: AtomicBool,
    /// If not 0, a streamed put stops reading its body after this many chunks and stores what
    /// it has read, as a backend that does not read to the end of the body does.
    pub(in crate::server) stop_after: AtomicUsize,
    /// Every HEAD that finds its object says it is one byte larger than it is.
    pub(in crate::server) lie_size: AtomicBool,
    /// The gate that holds a HEAD that found its object, after it has read it.
    heads: Mutex<Option<Arc<Semaphore>>>,
    /// The HEADs that wait at `heads`, counted when they arrive.
    heading: AtomicU64,
}

/// A semaphore that reads wait on, and the key whose reads wait (every key if none).
type Gate = (Option<String>, Arc<Semaphore>);

impl Tamper {
    fn new() -> Self {
        Self {
            memory: MemoryBackend::new(),
            short: AtomicBool::new(false),
            long: AtomicBool::new(false),
            blank_tags: AtomicBool::new(false),
            fail_once: AtomicBool::new(false),
            panic_once: AtomicBool::new(false),
            gate: Mutex::new(None),
            after: Mutex::new(None),
            parked: AtomicU64::new(0),
            deletes: Mutex::new(None),
            deleting: AtomicU64::new(0),
            lose_delete_answer: AtomicBool::new(false),
            needs_len: AtomicBool::new(false),
            stop_after: AtomicUsize::new(0),
            lie_size: AtomicBool::new(false),
            heads: Mutex::new(None),
            heading: AtomicU64::new(0),
        }
    }

    /// Holds every HEAD that finds its object from now on, after it has the answer, until a
    /// permit is added to the gate that is returned (a HEAD of a key that is not there is not
    /// held).
    pub(in crate::server) fn hold_heads(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        let mut held = self.heads.lock().unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::clone(&gate));
        gate
    }

    /// How many HEADs wait at the gate of [`hold_heads`](Self::hold_heads).
    pub(in crate::server) fn heading(&self) -> u64 {
        self.heading.load(Ordering::SeqCst)
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

    async fn put_stream(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<(), Error> {
        let stop = self.stop_after.load(Ordering::SeqCst);
        let (mut whole, mut chunks) = (BytesMut::new(), 0);
        while let Some(chunk) = body.next().await {
            whole.extend_from_slice(&chunk?);
            chunks += 1;
            if stop != 0 && chunks >= stop {
                break;
            }
        }
        self.memory.put(key, whole.freeze()).await
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
        if self.long.load(Ordering::SeqCst) {
            let mut more = body.to_vec();
            more.push(0);
            return Ok(Bytes::from(more));
        }
        Ok(body)
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let mut meta = self.memory.head(key).await?;
        if self.blank_tags.load(Ordering::SeqCst) {
            meta.tag.clear();
        }
        if self.lie_size.load(Ordering::SeqCst) {
            meta.size += 1;
        }
        let gate = self
            .heads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(gate) = gate {
            let _ = self.heading.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.expect("the gate stays open").forget();
        }
        Ok(meta)
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
        self.memory.delete(key).await?;
        if self.lose_delete_answer.swap(false, Ordering::SeqCst) {
            return Err(Error::Unavailable("the answer was lost".to_owned()));
        }
        Ok(())
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
    /// The gate that holds every write to a stage file, once set.
    write_gate: Mutex<Option<Arc<Semaphore>>>,
    /// The gate that holds every `link`, once set (the thread that calls it waits).
    link_gate: Mutex<Option<Arc<BlockingGate>>>,
    /// The gate that holds every `space`, once set (the thread that calls it waits).
    space_gate: Mutex<Option<Arc<BlockingGate>>>,
    /// The calls of `space` that wait at `space_gate`, counted when they arrive.
    space_waiting: AtomicU64,
    /// The names of every call of `link`, in order: from and to.
    pub(in crate::server) linked: Mutex<Vec<(PathBuf, PathBuf)>>,
    /// What a test has every call of `touch` do, after it is noted.
    pub(in crate::server) touch_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl FakeDisk {
    pub(in crate::server) fn new() -> Self {
        Self {
            quirk: Mutex::new(Quirk::None),
            space: Mutex::new(None),
            touched: Mutex::new(Vec::new()),
            links_made: Mutex::new(0),
            write_gate: Mutex::new(None),
            link_gate: Mutex::new(None),
            space_gate: Mutex::new(None),
            space_waiting: AtomicU64::new(0),
            linked: Mutex::new(Vec::new()),
            touch_hook: Mutex::new(None),
        }
    }

    /// Holds every write to a stage file from now on until a permit is added to the gate that is
    /// returned: a disk that hangs.
    pub(in crate::server) fn hold_writes(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        let mut held = self
            .write_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::clone(&gate));
        gate
    }

    /// Holds every `link` from now on, on its thread, until the gate that is returned is opened.
    pub(in crate::server) fn hold_links(&self) -> Arc<BlockingGate> {
        let gate = Arc::new(BlockingGate::new());
        let mut held = self
            .link_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::clone(&gate));
        gate
    }

    /// Holds every `space` from now on, on its thread (the eviction pass asks for it), until the
    /// gate that is returned is opened.
    pub(in crate::server) fn hold_space(&self) -> Arc<BlockingGate> {
        let gate = Arc::new(BlockingGate::new());
        let mut held = self
            .space_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *held = Some(Arc::clone(&gate));
        gate
    }

    /// How many calls of `space` wait at the gate of [`hold_space`](Self::hold_space).
    pub(in crate::server) fn space_waiting(&self) -> u64 {
        self.space_waiting.load(Ordering::SeqCst)
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

/// A gate that a thread (not a task) waits at until a test opens it: the calls of `Disk` that
/// are made on threads that may block.
pub(in crate::server) struct BlockingGate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl BlockingGate {
    fn new() -> Self {
        Self {
            open: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        while !*open {
            open = self
                .changed
                .wait(open)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Lets every thread at the gate go, and every one that comes.
    pub(in crate::server) fn open(&self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

impl Disk for FakeDisk {
    fn space(&self, path: &Path) -> io::Result<Space> {
        let gate = self
            .space_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(gate) = gate {
            let _ = self.space_waiting.fetch_add(1, Ordering::SeqCst);
            gate.wait();
        }
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
        let gate = self
            .link_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(gate) = gate {
            gate.wait();
        }
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

    fn write_stage<'a>(
        &'a self,
        file: &'a mut tokio::fs::File,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, io::Result<()>> {
        async move {
            let gate = self
                .write_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(gate) = gate {
                gate.acquire().await.expect("the gate stays open").forget();
            }
            tokio::io::AsyncWriteExt::write_all(file, bytes).await
        }
        .boxed()
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
        let hook = self
            .touch_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook();
        }
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
        never_expires(&mut config);
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

    /// A cache over the same directory that takes its lock, as the binary's does: read-write if
    /// nobody holds the lock, else read-only. [`Rig::cache`] takes none.
    pub(in crate::server) fn locked(&self) -> LocalCache {
        self.try_locked().expect("a cache")
    }

    /// The same, with the settings changed by `change`.
    pub(in crate::server) fn locked_with(
        &self,
        change: impl FnOnce(&mut CacheConfig),
    ) -> LocalCache {
        let mut config = self.config.clone();
        change(&mut config);
        Self::open_locked(&config, &self.upstream, &self.disk, &self.clock).expect("a cache")
    }

    /// The same, with the answer of the open.
    pub(in crate::server) fn try_locked(&self) -> io::Result<LocalCache> {
        let (config, upstream) = (&self.config, &self.upstream);
        Self::open_locked(config, upstream, &self.disk, &self.clock)
    }

    fn open_locked(
        config: &CacheConfig,
        upstream: &Arc<FaultyBackend>,
        disk: &Arc<FakeDisk>,
        clock: &Arc<TestClock>,
    ) -> io::Result<LocalCache> {
        let upstream: Arc<dyn Backend> = upstream.clone();
        let (disk, clock) = (disk.clone(), clock.clone());
        LocalCache::open_locked(config.clone(), upstream, disk, clock)
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

    /// Writes other bytes over an object the upstream holds (its `put` refuses a key that exists,
    /// so the old object is deleted first).
    pub(in crate::server) async fn replace(&self, key: &str, body: &[u8]) {
        self.tamper.delete(key).await.expect("a delete");
        self.put(key, body).await;
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

/// The most turns `until` gives up the thread before it fails the test. A count and not a time: an
/// event that never comes ends the test with a message, the same on a slow machine.
const SPINS: u32 = 1_000_000;

/// Waits, by giving up the thread, until `done` is true; fails the test, saying what `what` was
/// to have happened, after [`SPINS`] turns.
pub(in crate::server) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..SPINS {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("after {SPINS} turns of the scheduler, this never happened: {what}");
}
