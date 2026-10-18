//! The write pool (`kbf-front`'s `cache/pool.rs`): the blobs of ByteStream and storage
//! writes appended to shared segment files by a fixed number of writers, however many
//! uploads are in flight.
//!
//! Every test runs a [`Rig`]: one metadata log that outlives a restart, a faulty store,
//! and a tier directory on a `FaultFs` wrapped in a [`Counting`] filesystem, the tier's
//! file-open seam: it counts the files open for writing, their syncs and their writes, can
//! pause the syncs and can fail the writes or syncs. Each test's comment names the defect
//! it is there to catch.

mod common;
#[path = "write_tier/support.rs"]
#[allow(
    dead_code,
    reason = "shared with write_tier.rs, which uses the rest of it"
)]
mod support;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_front::{Cache, CacheError, PoolConfig, StreamedRecord, TierStartup};
use kbf_meta::{BlobAnswer, ObjectId};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_store::{FaultFs, Fs, FsFile};
use kbf_tier::{TierOptions, block_name};
use support::{Log, SEGMENT, SERVER, TestBlob, TierCache};

use common::faulty::FaultyStore;

const GIB: u64 = 1 << 30;
const MIB: usize = 1 << 20;

/// What the filesystem seam counts and does. Shared by every clone and every file.
#[derive(Debug, Default)]
struct Probe {
    /// Files opened for writing and not yet dropped, and the most there were at once.
    open: AtomicUsize,
    peak: AtomicUsize,
    /// File syncs that completed, and bytes written.
    syncs: AtomicUsize,
    written: AtomicU64,
    fail_writes: AtomicBool,
    fail_syncs: AtomicBool,
    fail_creates: AtomicBool,
    gate: Mutex<Gate>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Gate {
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
struct Counting {
    inner: FaultFs,
    probe: Arc<Probe>,
}

#[derive(Debug)]
struct CountedFile {
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
    fn new(inner: FaultFs) -> Self {
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
    fn pause(&self) {
        self.probe.gate().paused = true;
    }

    fn resume(&self) {
        self.probe.gate().paused = false;
        self.probe.changed.notify_all();
    }

    fn waiting(&self) -> usize {
        self.probe.gate().waiting
    }

    fn syncs(&self) -> usize {
        self.probe.syncs.load(Ordering::SeqCst)
    }

    fn peak(&self) -> usize {
        self.probe.peak.load(Ordering::SeqCst)
    }
}

/// One farm: a log and a store that outlive a restart, and a tier directory.
struct Rig {
    log: Log,
    store: FaultyStore,
    fs: Counting,
}

impl Drop for Rig {
    /// A test that fails while the syncs are paused must not leave the writers (and the
    /// runtime waiting for them) stuck.
    fn drop(&mut self) {
        self.fs.resume();
    }
}

impl Rig {
    fn new() -> Self {
        Self {
            log: Log::new(),
            store: FaultyStore::new(Capabilities::default()),
            fs: Counting::new(FaultFs::new()),
        }
    }

    /// A start over this rig's log and store on `fs`, with a tier of `options` and a
    /// write pool of `pool`.
    async fn start_on(
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

    async fn start(&self, writers: usize, queue: usize) -> Arc<TierCache> {
        let pool = pool(writers, queue);
        self.start_on(&self.fs, TierOptions::new(GIB), pool).await.0
    }

    /// Whether `digest` is staged on the tier (not in the store).
    fn on_tier(&self, digest: &kbf_types::Digest) -> bool {
        self.log.located(digest).1.is_some()
    }
}

fn pool(writers: usize, queue: usize) -> PoolConfig {
    PoolConfig {
        writers: writers.try_into().expect("writers"),
        queue: queue.try_into().expect("queue"),
        ..PoolConfig::DEFAULT
    }
}

/// A tier whose blocks hold 1 MiB, so a 5 MiB stream has a block of its own.
fn small_blocks() -> TierOptions {
    let mut options = TierOptions::new(GIB);
    options.segment_limit = MIB as u64;
    options
}

/// How long a test that must not hang may take.
const BOUND: Duration = Duration::from_secs(20);

/// Runs `body` on its own runtime and fails the test, with `what` in the message, if it
/// has not finished within [`BOUND`]. A leaked writer or a blocked send otherwise hangs the
/// test (and the runtime's shutdown, which waits for blocking calls). The clock is the
/// test thread's, not the runtime's: blocked sends can hold every runtime worker, and
/// then no tokio timer fires. The runtime is shut down without waiting.
fn bounded(what: &str, body: impl std::future::Future<Output = ()> + Send + 'static) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    let (done, finished) = std::sync::mpsc::channel();
    let task = runtime.spawn(body);
    runtime.spawn(async move {
        let _ = done.send(task.await);
    });
    let outcome = finished.recv_timeout(BOUND);
    runtime.shutdown_background();
    match outcome {
        Err(_) => panic!("{what}: not finished within {BOUND:?}: a writer or a send is stuck"),
        Ok(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Ok(Err(e)) => panic!("{what}: {e}"),
        Ok(Ok(())) => {}
    }
}

fn pooled(cache: TierCache, config: PoolConfig) -> TierCache {
    cache.with_write_pool(config)
}

/// Waits until `ready` holds.
async fn until(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..2000 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A blob streamed in 64 KiB pieces and ended as its digest: ready to commit.
async fn prepare(
    cache: &Arc<TierCache>,
    blob: &TestBlob,
) -> Result<StreamedRecord<support::Log, FaultyStore>, CacheError> {
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

async fn put(cache: &Arc<TierCache>, blob: &TestBlob) -> Result<(), CacheError> {
    prepare(cache, blob).await?.commit().await
}

async fn reads_back(cache: &TierCache, blob: &TestBlob) {
    let read = cache.read_blob(&blob.digest).await.expect("read");
    assert_eq!(read, blob.bytes, "the bytes of {}", blob.digest);
}

fn metric(cache: &TierCache, name: &str) -> u64 {
    let text = cache.render_metrics();
    let line = text
        .lines()
        .find(|l| l.split(' ').next() == Some(name))
        .unwrap_or_else(|| panic!("{name} is not printed"));
    line.rsplit(' ')
        .next()
        .expect("value")
        .parse()
        .expect("number")
}

/// Catches a file per blob: 200 uploads all received (every stream open and holding what
/// it needs) before any commits, through 4 writers. The files open for writing never pass
/// the writers plus the open grouped block, and every blob reads back as its bytes. Before
/// the pool each stream held a block file of its own from `stream` to its commit: 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_uploads_through_four_writers_open_at_most_five_files() {
    let rig = Rig::new();
    let cache = rig.start(4, 4096).await;
    let blobs = support::blobs(1, 200, 100_000);
    let mut received = Vec::new();
    for blob in &blobs {
        received.push(prepare(&cache, blob).await.expect("received"));
    }
    let commits = received.into_iter().map(StreamedRecord::commit);
    for committed in futures::future::join_all(commits).await {
        committed.expect("committed");
    }
    assert!(
        rig.fs.peak() <= 5,
        "{} files were open at once",
        rig.fs.peak()
    );
    assert!(rig.fs.peak() >= 1, "the seam counted no block file");
    for blob in &blobs {
        assert!(rig.on_tier(&blob.digest));
        reads_back(&cache, blob).await;
    }
    assert!(metric(&cache, "kbf_storage_open_write_fds") <= 5);
    assert_eq!(metric(&cache, "kbf_storage_write_pool_queue"), 0);
}

/// Catches a blob over the buffered size taking a file of its own, or a stream that
/// deadlocks when writers are fewer than uploads: twelve 5 MiB blobs, each streamed to a
/// writer as it arrives, through 2 writers. The open files stay at most 3, nothing waits for
/// ever, and every blob reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streamed_blobs_share_two_writers() {
    let rig = Rig::new();
    let cache = rig.start(2, 64).await;
    let blobs = support::blobs(300, 12, 5 * MIB);
    let puts = blobs.iter().map(|b| put(&cache, b));
    for done in futures::future::join_all(puts).await {
        done.expect("stored");
    }
    assert!(
        rig.fs.peak() <= 3,
        "{} files were open at once",
        rig.fs.peak()
    );
    for blob in &blobs {
        reads_back(&cache, blob).await;
    }
}

/// Catches an acknowledgement before the sync (the RAM-ack this PR does not make): with
/// the file sync paused, a committed blob is not answered and not present; once the sync
/// is released it is answered, present and readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ack_waits_for_the_sync() {
    let rig = Rig::new();
    let cache = rig.start(1, 16).await;
    let blob = TestBlob::new(1, 10_000);
    let record = prepare(&cache, &blob).await.expect("received");
    rig.fs.pause();
    let committing = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!committing.is_finished(), "acknowledged before the sync");
    assert_eq!(rig.log.answer(&blob.digest), BlobAnswer::Absent);
    rig.fs.resume();
    committing.await.expect("join").expect("committed");
    assert!(rig.on_tier(&blob.digest));
    assert_eq!(rig.fs.syncs(), 1);
    reads_back(&cache, &blob).await;
}

/// The same for a streamed blob: its acknowledgement waits for the sync of the writer
/// that took it, and until then the blob is absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_streamed_blobs_ack_waits_for_the_sync() {
    let rig = Rig::new();
    let cache = rig.start(1, 16).await;
    let blob = TestBlob::new(2, 5 * MIB);
    let record = prepare(&cache, &blob).await.expect("received");
    rig.fs.pause();
    let committing = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!committing.is_finished(), "acknowledged before the sync");
    assert_eq!(rig.log.answer(&blob.digest), BlobAnswer::Absent);
    rig.fs.resume();
    committing.await.expect("join").expect("committed");
    reads_back(&cache, &blob).await;
}

/// Catches blobs that wait behind a busy writer each costing their own sync and their
/// own `PutStaged` (group commit lost), a duplicate digest in a batch written or listed
/// twice, or a blob of a batch left unanswered: the one writer is held at its sync with
/// one blob, five distinct blobs and a repeat of one of them queue behind it; released,
/// they share one sync and one `PutStaged`, so two of each in all, and all six commits
/// answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blobs_queued_behind_a_writer_share_one_sync_and_one_put_staged() {
    let rig = Rig::new();
    let cache = rig.start(1, 64).await;
    let first = TestBlob::new(3, 20_000);
    let record = prepare(&cache, &first).await.expect("received");
    rig.fs.pause();
    let lead = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    let blobs = support::blobs(10, 5, 20_000);
    let mut queued = Vec::new();
    for blob in blobs.iter().chain(&blobs[..1]) {
        queued.push(tokio::spawn(
            prepare(&cache, blob).await.expect("received").commit(),
        ));
    }
    until("six blobs to queue", || {
        cache.metrics().write_pool_queue.get() == 6
    })
    .await;
    assert_eq!(metric(&cache, "kbf_storage_write_pool_queue"), 6);
    rig.fs.resume();
    lead.await.expect("join").expect("committed");
    for done in queued {
        done.await.expect("join").expect("committed");
    }
    assert_eq!(
        rig.fs.syncs(),
        2,
        "one sync for the first blob, one for the batch"
    );
    assert_eq!(rig.log.applied("put_staged"), 2);
    assert_eq!(rig.fs.peak(), 1, "one block for all of them");
    for blob in blobs.iter().chain([&first]) {
        reads_back(&cache, blob).await;
    }
    let object = rig.log.object_of(&first.digest);
    assert!(blobs.iter().all(|b| rig.log.object_of(&b.digest) == object));
}

/// Catches a batch with no bound, or one that loses the blob that did not fit: five
/// blobs of 4 MiB queue behind a held writer; the batch takes four (16 MiB) and the fifth
/// starts the next batch, so three syncs in all. A streamed blob queued among small ones
/// ends the batch, is written by its writer after it, and the small blob queued behind it
/// is written after that: all four commit and read back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_ends_at_its_byte_limit_and_at_a_stream() {
    let rig = Rig::new();
    let cache = rig.start(1, 64).await;
    let lead = TestBlob::new(4, 10_000);
    let record = prepare(&cache, &lead).await.expect("received");
    rig.fs.pause();
    let leading = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    let big = support::blobs(20, 5, 4 * MIB);
    let mut queued = Vec::new();
    for blob in &big {
        queued.push(tokio::spawn(
            prepare(&cache, blob).await.expect("received").commit(),
        ));
    }
    until("five blobs to queue", || {
        cache.metrics().write_pool_queue.get() == 5
    })
    .await;
    rig.fs.resume();
    leading.await.expect("join").expect("committed");
    for done in queued {
        done.await.expect("join").expect("committed");
    }
    assert_eq!(rig.fs.syncs(), 3, "the first, four in a batch, the fifth");
    for blob in big.iter().chain([&lead]) {
        reads_back(&cache, blob).await;
    }

    // A streamed blob among small ones.
    let lead = TestBlob::new(5, 10_000);
    let record = prepare(&cache, &lead).await.expect("received");
    rig.fs.pause();
    let leading = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    let small = support::blobs(30, 2, 10_000);
    let large = TestBlob::new(40, 5 * MIB);
    let after = TestBlob::new(41, 10_000);
    let before = prepare(&cache, &small[0]).await.expect("received");
    let also = prepare(&cache, &small[1]).await.expect("received");
    let (before, also) = (tokio::spawn(before.commit()), tokio::spawn(also.commit()));
    until("two blobs to queue", || {
        cache.metrics().write_pool_queue.get() == 2
    })
    .await;
    // The stream queues when it opens; its chunks wait for its writer.
    let streaming = {
        let (cache, large) = (Arc::clone(&cache), large.clone());
        tokio::spawn(async move { put(&cache, &large).await })
    };
    until("the stream to queue", || {
        cache.metrics().write_pool_queue.get() == 3
    })
    .await;
    let behind = tokio::spawn(prepare(&cache, &after).await.expect("received").commit());
    until("the last blob to queue", || {
        cache.metrics().write_pool_queue.get() == 4
    })
    .await;
    rig.fs.resume();
    leading.await.expect("join").expect("committed");
    for done in [before, also, behind] {
        done.await.expect("join").expect("committed");
    }
    streaming.await.expect("join").expect("stored");
    for blob in [&lead, &small[0], &small[1], &large, &after] {
        reads_back(&cache, blob).await;
    }
}

/// Catches a batch with no count bound (one sync and one `PutStaged` listing every blob
/// waiting, however many) or one that drops the blobs past the bound: 300 small blobs queue
/// behind a held writer; they are written in a batch of 256 (`BATCH_BLOBS`) and one of 44, so
/// three syncs in all with the first blob's, and every blob reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_ends_at_its_blob_limit() {
    let rig = Rig::new();
    let cache = rig.start(1, 1024).await;
    let lead = TestBlob::new(90, 1000);
    let record = prepare(&cache, &lead).await.expect("received");
    rig.fs.pause();
    let leading = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    let blobs = support::blobs(100, 300, 100);
    let mut queued = Vec::new();
    for blob in &blobs {
        queued.push(tokio::spawn(
            prepare(&cache, blob).await.expect("received").commit(),
        ));
    }
    until("300 blobs to queue", || {
        cache.metrics().write_pool_queue.get() == 300
    })
    .await;
    rig.fs.resume();
    leading.await.expect("join").expect("committed");
    for done in queued {
        done.await.expect("join").expect("committed");
    }
    assert_eq!(
        rig.fs.syncs(),
        3,
        "the first, 256 in a batch, 44 in the next"
    );
    assert_eq!(rig.log.applied("put_staged"), 3);
    for blob in &blobs {
        reads_back(&cache, blob).await;
    }
}

/// Catches a full queue that blocks for ever or errors, instead of sending the blob to the
/// store: one writer held at its sync, a queue of one. The blob in the writer and the
/// blob queued are staged once it is released. A blob opened when the queue is full goes
/// straight to the store and is present without waiting for the writer; one committed
/// after the queue filled does too. Neither errors, and the metric counts both.
#[test]
fn a_full_queue_sends_the_blob_to_the_store_without_an_error() {
    bounded(
        "a_full_queue_sends_the_blob_to_the_store_without_an_error",
        async {
            let rig = Rig::new();
            let cache = rig.start(1, 1).await;
            let held = TestBlob::new(6, 10_000);
            let held_record = prepare(&cache, &held).await.expect("received");
            // Opened while the queue is empty, committed after it is full.
            let late = TestBlob::new(7, 10_000);
            let late_record = prepare(&cache, &late).await.expect("received");
            rig.fs.pause();
            let holding = tokio::spawn(held_record.commit());
            until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
            let queued = TestBlob::new(8, 10_000);
            let queued_record = prepare(&cache, &queued).await.expect("received");
            let queueing = tokio::spawn(queued_record.commit());
            until("one blob to queue", || {
                cache.metrics().write_pool_queue.get() == 1
            })
            .await;

            // Committed with the queue full: straight to the store, answered at once.
            late_record.commit().await.expect("not an error");
            assert!(!rig.on_tier(&late.digest), "{} was queued", late.digest);
            reads_back(&cache, &late).await;
            // Opened with the queue full: streamed straight to the store from its first chunk.
            let direct = TestBlob::new(9, 10_000);
            put(&cache, &direct).await.expect("not an error");
            assert!(!rig.on_tier(&direct.digest));
            reads_back(&cache, &direct).await;
            assert_eq!(metric(&cache, "kbf_storage_write_pool_direct_total"), 2);
            assert_eq!(rig.log.answer(&held.digest), BlobAnswer::Absent);

            rig.fs.resume();
            holding.await.expect("join").expect("committed");
            queueing.await.expect("join").expect("committed");
            assert!(rig.on_tier(&held.digest) && rig.on_tier(&queued.digest));
            // A large blob finding the queue full goes to the store too.
            rig.fs.pause();
            let again = tokio::spawn({
                let (cache, held) = (Arc::clone(&cache), TestBlob::new(11, 10_000));
                async move { put(&cache, &held).await }
            });
            until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
            let queued = TestBlob::new(12, 10_000);
            let queueing = tokio::spawn(prepare(&cache, &queued).await.expect("received").commit());
            until("one blob to queue", || {
                cache.metrics().write_pool_queue.get() == 1
            })
            .await;
            let large = TestBlob::new(13, 5 * MIB);
            put(&cache, &large).await.expect("not an error");
            assert!(!rig.on_tier(&large.digest));
            reads_back(&cache, &large).await;
            rig.fs.resume();
            again.await.expect("join").expect("stored");
            queueing.await.expect("join").expect("committed");
        },
    );
}

/// Catches a tier with no room stalling or failing a write: a blob the tier's budget
/// cannot hold goes to the store whether it is held whole (2 MiB) or streamed (5 MiB), and
/// is counted; one that fits is staged. A tier whose files cannot be created takes no
/// blob either: a held blob is handed to a writer that cannot reserve it, and goes to
/// the store when it answers; a streamed blob is refused when its writer takes it, so its
/// first chunk goes to the store. The next blob, with the disk back, is staged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tier_without_room_sends_the_blob_to_the_store() {
    let rig = Rig::new();
    let (cache, _) = rig
        .start_on(&rig.fs, TierOptions::new(MIB as u64), pool(2, 16))
        .await;
    let whole = TestBlob::new(14, 2 * MIB);
    put(&cache, &whole).await.expect("stored");
    assert!(!rig.on_tier(&whole.digest));
    reads_back(&cache, &whole).await;
    let streamed = TestBlob::new(15, 5 * MIB);
    put(&cache, &streamed).await.expect("stored");
    assert!(!rig.on_tier(&streamed.digest));
    reads_back(&cache, &streamed).await;
    assert_eq!(metric(&cache, "kbf_storage_write_pool_direct_total"), 2);
    assert_eq!(metric(&cache, "kbf_write_tier_full_total"), 2);
    let small = TestBlob::new(16, 1000);
    put(&cache, &small).await.expect("stored");
    assert!(rig.on_tier(&small.digest), "a blob that fits is staged");

    let rig = Rig::new();
    let cache = rig.start(2, 16).await;
    rig.fs.probe.fail_creates.store(true, Ordering::SeqCst);
    let whole = TestBlob::new(17, 10_000);
    put(&cache, &whole).await.expect("stored");
    assert!(!rig.on_tier(&whole.digest));
    reads_back(&cache, &whole).await;
    let streamed = TestBlob::new(18, 5 * MIB);
    put(&cache, &streamed).await.expect("stored");
    assert!(!rig.on_tier(&streamed.digest));
    reads_back(&cache, &streamed).await;
    assert_eq!(metric(&cache, "kbf_storage_write_pool_direct_total"), 2);
    rig.fs.probe.fail_creates.store(false, Ordering::SeqCst);
    let next = TestBlob::new(19, 10_000);
    put(&cache, &next).await.expect("stored");
    assert!(rig.on_tier(&next.digest));
}

/// Catches a blob larger than a shared block being refused, or given a file outside the
/// pool: with the tier's segment limit at 1 MiB, a 5 MiB stream cannot share a block, is
/// written to a block of its own by a writer, and is staged, readable, with at most the
/// writers' files open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blob_over_the_segment_limit_gets_a_block_of_its_own_through_a_writer() {
    let rig = Rig::new();
    let (cache, _) = rig.start_on(&rig.fs, small_blocks(), pool(2, 16)).await;
    let blobs = support::blobs(50, 3, 5 * MIB);
    for blob in &blobs {
        put(&cache, blob).await.expect("stored");
        assert!(rig.on_tier(&blob.digest));
        reads_back(&cache, blob).await;
    }
    assert!(rig.fs.peak() <= 3, "{} files were open", rig.fs.peak());
}

/// Catches a stream dropped before its commit keeping its writer, or leaving its bytes
/// visible: with one writer, a 5 MiB stream that is dropped after two chunks is absent,
/// and the next blob is written at once (the writer is free).
#[test]
fn an_abandoned_stream_frees_its_writer_and_stores_nothing() {
    bounded(
        "an_abandoned_stream_frees_its_writer_and_stores_nothing",
        async {
            let rig = Rig::new();
            let cache = rig.start(1, 16).await;
            let dropped = TestBlob::new(17, 5 * MIB);
            {
                let (cache, dropped) = (Arc::clone(&cache), dropped.clone());
                tokio::task::spawn_blocking(move || {
                    let mut stream = cache.stream(dropped.bytes.len() as u64).expect("a stream");
                    for piece in dropped.bytes.chunks(MIB).take(2) {
                        stream.append(piece).expect("appended");
                    }
                })
                .await
                .expect("join");
            }
            let next = TestBlob::new(18, 10_000);
            put(&cache, &next).await.expect("stored");
            assert!(rig.on_tier(&next.digest));
            assert_eq!(rig.log.answer(&dropped.digest), BlobAnswer::Absent);
            // And a stream ended as a digest it does not have stores nothing.
            let wrong = TestBlob::new(19, 5 * MIB);
            let liar = {
                let (cache, wrong) = (Arc::clone(&cache), wrong.clone());
                tokio::task::spawn_blocking(move || {
                    let mut stream = cache.stream(wrong.bytes.len() as u64).expect("a stream");
                    for piece in wrong.bytes.chunks(MIB) {
                        stream.append(piece).expect("appended");
                    }
                    stream.finish(next.digest, 0).map(drop)
                })
                .await
                .expect("join")
            };
            assert!(matches!(liar, Err(CacheError::Invalid(_))), "{liar:?}");
            assert_eq!(rig.log.answer(&wrong.digest), BlobAnswer::Absent);
            put(&cache, &TestBlob::new(21, 10_000))
                .await
                .expect("the writer is free");
        },
    );
}

/// Catches a failed `PutStaged` answering some of a batch, or leaving the writer stuck: the
/// log refuses the commit of a batch, so every blob in it fails (a held whole blob, and a
/// streamed one), nothing of them is present, and the next blobs are written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_commit_fails_the_whole_batch_and_the_writer_goes_on() {
    let rig = Rig::new();
    let cache = rig.start(1, 16).await;
    let lead = TestBlob::new(22, 10_000);
    let record = prepare(&cache, &lead).await.expect("received");
    rig.fs.pause();
    let leading = tokio::spawn(record.commit());
    until("the writer to reach the sync", || rig.fs.waiting() == 1).await;
    let batch = support::blobs(60, 3, 10_000);
    let mut queued = Vec::new();
    for blob in &batch {
        queued.push(tokio::spawn(
            prepare(&cache, blob).await.expect("received").commit(),
        ));
    }
    until("three blobs to queue", || {
        cache.metrics().write_pool_queue.get() == 3
    })
    .await;
    // The first commit is the leader's, the second the batch's.
    rig.log.refuse("put_staged", 2);
    rig.fs.resume();
    assert!(leading.await.expect("join").is_err(), "the leader");
    for (done, blob) in queued.into_iter().zip(&batch) {
        assert!(done.await.expect("join").is_err(), "{}", blob.digest);
        assert_eq!(rig.log.answer(&blob.digest), BlobAnswer::Absent);
    }
    assert_eq!(rig.log.answer(&lead.digest), BlobAnswer::Absent);
    // A batch of one and a stream each fail when the log refuses.
    rig.log.refuse("put_staged", 1);
    let held = TestBlob::new(23, 10_000);
    let refused = put(&cache, &held).await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(rig.log.answer(&held.digest), BlobAnswer::Absent);
    rig.log.refuse("put_staged", 1);
    let streamed = TestBlob::new(24, 5 * MIB);
    let refused = put(&cache, &streamed).await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(rig.log.answer(&streamed.digest), BlobAnswer::Absent);
    let next = TestBlob::new(25, 10_000);
    put(&cache, &next).await.expect("the writer goes on");
    assert!(rig.on_tier(&next.digest));
}

/// Catches a failed stream leaking its file: waits until both the filesystem seam and the
/// tier's own count (`kbf_storage_open_write_fds`, set after each pool step) show `want`
/// block files open for writing. A stream larger than a segment has a block of its own,
/// which closes when its reservation is dropped; what stays open is the grouped block.
async fn open_files_settle_at(rig: &Rig, cache: &TierCache, want: usize) {
    until("the block files to close", || {
        rig.fs.probe.open.load(Ordering::SeqCst) == want
    })
    .await;
    until("the tier to count the files", || {
        metric(cache, "kbf_storage_open_write_fds") == want as u64
    })
    .await;
}

/// Catches a disk error taken for a stored blob, or one that fails the upload instead of
/// sending a held blob to the store: with every write failing, a held blob goes to the
/// store (answered, present, readable); with every sync failing, the same; a streamed blob
/// whose chunk cannot be written fails its upload, and one whose sync fails fails its
/// commit, and neither is present.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disk_errors_send_held_blobs_to_the_store_and_fail_streamed_ones() {
    let rig = Rig::new();
    let (cache, _) = rig.start_on(&rig.fs, small_blocks(), pool(1, 16)).await;
    rig.fs.probe.fail_writes.store(true, Ordering::SeqCst);
    let held = TestBlob::new(26, 10_000);
    put(&cache, &held).await.expect("sent to the store");
    assert!(!rig.on_tier(&held.digest));
    reads_back(&cache, &held).await;
    let open_before = rig.fs.probe.open.load(Ordering::SeqCst);
    let streamed = TestBlob::new(27, 5 * MIB);
    let failed = put(&cache, &streamed).await;
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(rig.log.answer(&streamed.digest), BlobAnswer::Absent);
    // The writer dropped the stream's reservation: its block of its own is closed, and
    // only what was open before it (the grouped block) is (a leaked reservation keeps its
    // file open for ever).
    open_files_settle_at(&rig, &cache, open_before).await;
    rig.fs.probe.fail_writes.store(false, Ordering::SeqCst);

    rig.fs.probe.fail_syncs.store(true, Ordering::SeqCst);
    let held = TestBlob::new(28, 10_000);
    put(&cache, &held).await.expect("sent to the store");
    assert!(!rig.on_tier(&held.digest));
    let streamed = TestBlob::new(29, 5 * MIB);
    let failed = put(&cache, &streamed).await;
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(rig.log.answer(&streamed.digest), BlobAnswer::Absent);
    rig.fs.probe.fail_syncs.store(false, Ordering::SeqCst);
    let next = TestBlob::new(31, 10_000);
    put(&cache, &next).await.expect("the writer goes on");
    assert!(rig.on_tier(&next.digest));
}

/// Catches a restart that exposes a partial blob or loses the blobs before it in a shared
/// segment (the committed length not respected). Three blobs share one block. A fourth,
/// streamed, has two chunks written (past the committed length) when the power is cut with
/// those bytes on disk. After the restart the block is cut back to the third blob's end:
/// the three read back as their bytes, the fourth is absent, and no object is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_after_a_partial_append_leaves_the_committed_blobs_and_no_partial_one() {
    let rig = Rig::new();
    let cache = rig.start(2, 16).await;
    let blobs = support::blobs(70, 3, 20_000);
    for blob in &blobs {
        put(&cache, blob).await.expect("stored");
    }
    let object = rig.log.object_of(&blobs[0].digest);
    for blob in &blobs {
        assert_eq!(
            rig.log.object_of(&blob.digest),
            object,
            "one shared segment"
        );
    }
    let committed = rig
        .log
        .state(move |s| s.staged(object).map(|o| o.committed_len()))
        .expect("staged");
    let partial = TestBlob::new(75, 5 * MIB);
    let written = rig.fs.probe.written.load(Ordering::SeqCst);
    let appending = {
        let (cache, partial) = (Arc::clone(&cache), partial.clone());
        tokio::task::spawn_blocking(move || {
            let mut stream = cache.stream(partial.bytes.len() as u64).expect("a stream");
            for piece in partial.bytes.chunks(MIB).take(2) {
                stream.append(piece).expect("appended");
            }
            stream
        })
        .await
        .expect("join")
    };
    until("the chunks to reach the block", || {
        rig.fs.probe.written.load(Ordering::SeqCst) >= written + MIB as u64
    })
    .await;
    let name = block_name(object);
    let cut = Counting::new(rig.fs.inner.crash(usize::MAX));
    let on_disk = cut.inner.contents(&name).expect("the block").len() as u64;
    assert!(
        on_disk > committed,
        "the partial blob is on disk: {on_disk} <= {committed}"
    );
    drop(appending);
    drop(cache);

    let (cache, startup) = rig.start_on(&cut, TierOptions::new(GIB), pool(2, 16)).await;
    assert!(startup.recovery.lost.is_empty(), "{startup:?}");
    assert!(startup.recovery.truncated.contains(&object), "{startup:?}");
    assert_eq!(
        cut.inner.contents(&name).expect("block").len() as u64,
        committed
    );
    for blob in &blobs {
        reads_back(&cache, blob).await;
    }
    assert_eq!(rig.log.answer(&partial.digest), BlobAnswer::Absent);
    // The restarted pool writes on.
    let next = TestBlob::new(76, 20_000);
    put(&cache, &next).await.expect("stored");
    reads_back(&cache, &next).await;
}

/// Catches pooled blobs that the flusher cannot send on: a block shared by many blobs,
/// sealed and flushed, is one object in the store, and every blob in it still reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_block_is_flushed_as_one_object() {
    let rig = Rig::new();
    let cache = rig.start(4, 64).await;
    let blobs = support::blobs(80, 20, 30_000);
    let puts = blobs.iter().map(|b| put(&cache, b));
    for done in futures::future::join_all(puts).await {
        done.expect("stored");
    }
    let objects: std::collections::BTreeSet<ObjectId> =
        blobs.iter().map(|b| rig.log.object_of(&b.digest)).collect();
    assert!(objects.len() <= 5, "{} blocks for 20 blobs", objects.len());
    let round = cache.flush_tier(support::later()).await.expect("round");
    assert_eq!(round.flushed.len(), objects.len(), "{round:?}");
    assert_eq!(rig.store.put_keys().len(), objects.len());
    for blob in &blobs {
        reads_back(&cache, blob).await;
    }
}

/// The text logged so far by the process's one global subscriber, installed on first use.
/// Global, not per-thread: the pool's writer threads log, not the test's.
fn captured_log() -> &'static Mutex<Vec<u8>> {
    static LOG: std::sync::OnceLock<&'static Mutex<Vec<u8>>> = std::sync::OnceLock::new();
    struct Sink(&'static Mutex<Vec<u8>>);
    impl io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    LOG.get_or_init(|| {
        let log: &'static Mutex<Vec<u8>> = Box::leak(Box::default());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || Sink(log))
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");
        log
    })
}

/// The idle bound is measured from the last chunk, not from the job's start: a stream
/// whose chunks come every 0.6 x idle (never a stall) for over 2 x idle in all commits and
/// reads back. Catches a total-lifetime timeout, which would kill every healthy upload
/// longer than the idle time.
#[test]
fn a_slow_but_steady_stream_outlives_the_idle_time() {
    bounded("a_slow_but_steady_stream_outlives_the_idle_time", async {
        let rig = Rig::new();
        let idle = Duration::from_millis(300);
        let config = PoolConfig {
            stream_idle: idle,
            ..pool(1, 16)
        };
        let cache = rig.start_on(&rig.fs, small_blocks(), config).await.0;
        let blob = TestBlob::new(42, 5 * MIB);
        let started = std::time::Instant::now();
        let upload = {
            let (cache, blob) = (Arc::clone(&cache), blob.clone());
            tokio::task::spawn_blocking(move || {
                let mut stream = cache.stream(blob.bytes.len() as u64).expect("a stream");
                for piece in blob.bytes.chunks(MIB) {
                    stream.append(piece)?;
                    std::thread::sleep(idle.mul_f64(0.6));
                }
                stream.finish(blob.digest, kbf_segments::crc32c(&blob.bytes))
            })
        };
        let prepared = upload
            .await
            .expect("join")
            .expect("a steady stream is not a stall");
        assert!(
            started.elapsed() > idle * 2,
            "the stream did not outlast twice the idle time: {:?}",
            started.elapsed()
        );
        prepared.commit().await.expect("committed");
        assert!(rig.on_tier(&blob.digest));
        reads_back(&cache, &blob).await;
    });
}

/// Catches a stalled but connected upload holding a writer for ever: with one writer, a
/// 5 MiB stream sends one chunk and then nothing, and a small blob is queued behind it.
/// The small blob waits while the stream is within its idle time (so the test would see
/// the head-of-line blocking), is then acknowledged and on the tier, once the idle time
/// (300 ms) has passed, and not much later; the stalled upload's next call fails
/// UNAVAILABLE (retryable, not INVALID_ARGUMENT); and its block of its own is closed (only
/// the grouped block the small blob went to is open). It also pins what an operator sees:
/// exactly one WARN, "a streamed blob sent no chunk in time; its writer is freed", with
/// `received` the bytes sent before the stall (1 MiB). The warning is logged on a writer
/// thread, so the capture is a global subscriber (a thread-local one would see nothing).
#[test]
fn a_stalled_stream_loses_its_writer_and_fails_retryably() {
    let log = captured_log();
    bounded(
        "a_stalled_stream_loses_its_writer_and_fails_retryably",
        async {
            let rig = Rig::new();
            let idle = Duration::from_millis(300);
            let config = PoolConfig {
                stream_idle: idle,
                ..pool(1, 16)
            };
            let cache = rig.start_on(&rig.fs, small_blocks(), config).await.0;
            let stalled = TestBlob::new(40, 5 * MIB);
            let (sent, sent_rx) = std::sync::mpsc::channel();
            let (go, go_rx) = std::sync::mpsc::channel::<()>();
            let upload = {
                let (cache, stalled) = (Arc::clone(&cache), stalled.clone());
                tokio::task::spawn_blocking(move || {
                    let mut stream = cache.stream(stalled.bytes.len() as u64).expect("a stream");
                    stream.append(&stalled.bytes[..MIB]).expect("appended");
                    sent.send(()).expect("told");
                    // Stalled: connected, and sending nothing, until the test lets it go on.
                    go_rx.recv().expect("told to go on");
                    stream.append(&stalled.bytes[MIB..2 * MIB])
                })
            };
            tokio::task::spawn_blocking(move || sent_rx.recv().expect("the first chunk"))
                .await
                .expect("join");
            let small = TestBlob::new(41, 10_000);
            let started = std::time::Instant::now();
            let putting = tokio::spawn({
                let (cache, small) = (Arc::clone(&cache), small.clone());
                async move { put(&cache, &small).await }
            });
            tokio::time::sleep(idle / 3).await;
            assert!(
                !putting.is_finished(),
                "the small blob was acknowledged while the writer was held"
            );
            putting.await.expect("join").expect("stored");
            let waited = started.elapsed();
            assert!(
                waited >= idle / 2 && waited < idle + Duration::from_secs(5),
                "the small blob waited {waited:?} for an idle time of {idle:?}"
            );
            assert!(rig.on_tier(&small.digest));
            reads_back(&cache, &small).await;
            go.send(()).expect("told");
            let failed = upload
                .await
                .expect("join")
                .expect_err("the stalled upload fails");
            assert!(matches!(failed, CacheError::Stalled(_)), "{failed:?}");
            assert_eq!(
                tonic::Status::from(failed).code(),
                tonic::Code::Unavailable,
                "retryable"
            );
            assert_eq!(rig.log.answer(&stalled.digest), BlobAnswer::Absent);
            // The stalled stream's block of its own is closed; the small blob's grouped block is not.
            open_files_settle_at(&rig, &cache, 1).await;
        },
    );
    let text = String::from_utf8(log.lock().unwrap_or_else(PoisonError::into_inner).clone())
        .expect("utf-8");
    let freed: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("a streamed blob sent no chunk in time; its writer is freed"))
        .collect();
    assert_eq!(freed.len(), 1, "one warning per freed writer: {text}");
    assert!(freed[0].contains("WARN"), "{}", freed[0]);
    assert!(
        freed[0].contains(&format!("received={MIB}")),
        "the bytes received before the stall: {}",
        freed[0]
    );
    assert!(freed[0].contains("idle=300ms"), "{}", freed[0]);
}
