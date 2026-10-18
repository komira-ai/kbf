//! The write pool: a fixed number of writers that append the blobs of ByteStream and
//! storage `Write`s to shared segment files (`docs/design/write-path.md`, section 6).
//!
//! Before the pool, every such blob was a block of its own: one file created, held open
//! while its bytes arrived, synced and sealed with its commit. The open write
//! descriptors grew with the uploads in flight (a burst of 2,200 actions reached the
//! 1,024 a node had). Now a blob is **handed to a writer**, one of
//! [`PoolConfig::writers`] threads started with the first blob, and a writer holds at most one
//! reservation in the tier at a time, in the open grouped block that every blob shares.
//!
//! - **A blob of at most [`BUFFERED_BLOB_BYTES`]** is received and hashed as before, held
//!   in memory while it arrives (the storage node takes room for it first), and handed to
//!   the pool whole when it is committed. A writer takes the first blob waiting and as many
//!   as are queued behind it (up to [`BATCH_BLOBS`] blobs and [`BATCH_BYTES`] bytes), writes them one
//!   after another in a single range of the grouped block, syncs it **once**, commits **one**
//!   `PutStaged` listing every blob, and only then answers each of them. A blob is answered
//!   after its bytes are synced and its `PutStaged` applied, as before the pool.
//! - **A larger blob** (every blob over [`MAX_UNSTAGED_BLOB_BYTES`](crate::MAX_UNSTAGED_BLOB_BYTES)
//!   is one) streams through a writer: it is queued when it opens, the writer reserves its range
//!   when it takes the job, and each chunk is written as it arrives. The writer is held
//!   until the blob is committed or abandoned, so a slow sender holds one writer, never more
//!   than one file.
//! - **Backpressure.** The queue holds [`PoolConfig::queue`] blobs. A blob that finds it full is
//!   not queued and not refused: it goes straight to the object store, as one does when the
//!   tier has no room ([`Cache::store_streamed`]). So does a blob the tier does not take
//!   (the budget is full, or the disk failed before the commit).
//!
//! **The descriptor bound.** The pool holds a file open only through a reservation, and a
//! writer holds one at a time, so the pool's blobs keep at most `writers + 1` block files
//! open for writing: one per writer in the worst case (each in a block of its own, still
//! being sealed), and the open grouped block. A stream whose blob is larger than the tier's
//! segment limit cannot share a block: it is written to a block of its own, still by one
//! writer, so the bound holds for it too. Not covered: the files other callers
//! write (a `Segments` upload's ranges, `Cache::store_blobs`' segments), which have their
//! own bounds.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_objstore::ObjectStore;
use kbf_tier::{Reservation, Synced, TierError, WriteTier};
use kbf_types::Digest;
use tokio::sync::oneshot;

use super::tier::Staging;
use super::write_group::Writer;
use super::{Cache, CacheError};
use crate::meta_log::MetaLog;
use crate::metrics::CacheMetrics;

/// Blobs of at most this many bytes are held in memory while they arrive and written in
/// a batch. The storage node takes room for `min(size, 4 MiB)` bytes before it receives a
/// blob, so these are exactly the bytes it counted.
pub const BUFFERED_BLOB_BYTES: u64 = 4 << 20;

/// The most blobs one writer syncs and commits together.
pub const BATCH_BLOBS: usize = 256;

/// The most bytes one writer syncs and commits together (a blob beyond it starts the
/// next batch).
pub const BATCH_BYTES: u64 = 16 << 20;

/// The chunks of a streaming blob waiting for its writer.
const STREAM_CHUNKS_IN_FLIGHT: usize = 2;

/// Why an append to a stream stopped.
const STOPPED: &str = "the write pool stopped taking the streamed blob";

/// How many writers the pool has and how many blobs may wait for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    /// The writers (`--write-pool-writers`): the most block files the pool's blobs keep open
    /// for writing, plus one (the open grouped block).
    pub writers: NonZeroUsize,
    /// The most blobs waiting for a writer (`--write-pool-queue`). A blob that finds the
    /// queue full goes straight to the object store.
    pub queue: NonZeroUsize,
}

impl PoolConfig {
    /// 8 writers, and room for 4,096 blobs behind them (at most 4 MiB each, under the
    /// storage node's `--write-bytes-in-flight`).
    pub const DEFAULT: Self = Self {
        writers: NonZeroUsize::MIN.saturating_add(7),
        queue: NonZeroUsize::MIN.saturating_add(4095),
    };
}

/// What a writer is asked to do.
enum Job {
    /// A blob held whole: batched with the others waiting.
    Whole(Whole),
    /// A blob arriving chunk by chunk.
    Stream(StreamJob),
}

struct Whole {
    digest: Digest,
    bytes: Bytes,
    done: oneshot::Sender<Outcome>,
}

/// How a [`Whole`] ended.
enum Outcome {
    /// Synced and committed.
    Staged,
    /// The tier did not take it (no room, or its disk failed before the commit): the
    /// caller still holds the bytes and sends them to the store.
    NoRoom,
    /// The commit failed.
    Failed(CacheError),
}

struct StreamJob {
    size: u64,
    /// Whether the writer took the blob: `false` if the tier had no room for it, and the
    /// sender sends it to the store instead. Nothing is sent before the answer.
    accept: oneshot::Sender<bool>,
    chunks: Receiver<Bytes>,
    /// The digest the blob is committed as, and where to answer.
    commit: oneshot::Receiver<(Digest, oneshot::Sender<Result<(), CacheError>>)>,
}

/// Where the blobs of a pool are queued for its writers.
#[derive(Clone, Debug)]
pub(crate) struct Jobs(SyncSender<Job>);

/// The queue the writers take from. Made with the first blob.
#[derive(Debug)]
pub(super) struct WritePool {
    jobs: Jobs,
    queue: usize,
}

impl WritePool {
    /// Starts `config.writers` threads over `writer`'s tier; `None` if no thread could be
    /// started.
    pub(super) fn start<M: MetaLog, O: ObjectStore>(
        writer: Arc<Writer<M, O>>,
        staging: &Arc<Staging>,
        runtime: tokio::runtime::Handle,
        config: PoolConfig,
    ) -> Option<Self> {
        let (jobs, queued) = mpsc::sync_channel(config.queue.get());
        let queued = Arc::new(Mutex::new(queued));
        let mut started = 0;
        for n in 0..config.writers.get() {
            let worker = Worker {
                writer: Arc::clone(&writer),
                staging: Arc::clone(staging),
                runtime: runtime.clone(),
                queued: Arc::clone(&queued),
            };
            let name = format!("kbf-write-pool-{n}");
            let spawned = std::thread::Builder::new()
                .name(name)
                .spawn(move || worker.run());
            started += usize::from(spawned.is_ok());
        }
        (started > 0).then_some(Self {
            jobs: Jobs(jobs),
            queue: config.queue.get(),
        })
    }

    fn full(&self, metrics: &CacheMetrics) -> bool {
        metrics.write_pool_queue.get() >= self.queue as u64
    }
}

/// One writer thread.
struct Worker<M, O> {
    writer: Arc<Writer<M, O>>,
    /// The tier the pool writes to.
    staging: Arc<Staging>,
    runtime: tokio::runtime::Handle,
    queued: Arc<Mutex<Receiver<Job>>>,
}

impl<M: MetaLog, O: ObjectStore> Worker<M, O> {
    /// Takes jobs until every sender is gone.
    fn run(self) {
        let mut carry = None;
        loop {
            let job = match carry.take() {
                Some(job) => job,
                None => match self.next() {
                    Some(job) => job,
                    None => return,
                },
            };
            match job {
                Job::Stream(stream) => self.stream(stream),
                Job::Whole(first) => {
                    let (batch, after) = self.gather(first);
                    carry = after;
                    self.stage(batch);
                }
            }
        }
    }

    fn metrics(&self) -> &CacheMetrics {
        &self.writer.metrics
    }

    /// The next job, waiting for one; `None` when no sender is left.
    fn next(&self) -> Option<Job> {
        let queued = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        let job = queued.recv().ok()?;
        self.metrics().write_pool_queue.sub_one();
        Some(job)
    }

    /// `first` and the blobs queued behind it that fit one batch, and the job that did
    /// not fit (the next batch's first, or a stream).
    fn gather(&self, first: Whole) -> (Vec<Whole>, Option<Job>) {
        // Not waited for: a writer idle in `next` holds the lock, and then nothing is
        // queued to gather.
        let Ok(queued) = self.queued.try_lock() else {
            return (vec![first], None);
        };
        let mut bytes = first.bytes.len() as u64;
        let mut batch = vec![first];
        while batch.len() < BATCH_BLOBS {
            let Ok(job) = queued.try_recv() else { break };
            self.metrics().write_pool_queue.sub_one();
            match job {
                Job::Whole(w) if bytes + w.bytes.len() as u64 <= BATCH_BYTES => {
                    bytes += w.bytes.len() as u64;
                    batch.push(w);
                }
                other => return (batch, Some(other)),
            }
        }
        (batch, None)
    }

    /// Writes `batch` after one another in one range, syncs it once, commits one
    /// `PutStaged`, and answers every blob: staged after the commit, or, if the tier did
    /// not take the range, told to go to the store.
    fn stage(&self, batch: Vec<Whole>) {
        let staging = &self.staging;
        // Equal digests in a batch are written once.
        let mut unique: BTreeMap<Digest, &Bytes> = BTreeMap::new();
        for blob in &batch {
            unique.entry(blob.digest).or_insert(&blob.bytes);
        }
        let written = self.write(&*staging.tier, &unique);
        self.note_files(&*staging.tier);
        let (synced, listed) = match written {
            Ok(written) => written,
            Err(e) => {
                staging.counters.direct(&e);
                tracing::debug!(error = %e, blobs = batch.len(), "the write tier did not take a batch; writing the blobs to the store");
                return self.send_to_store(batch);
            }
        };
        let committed = self.put_staged(synced, listed, false);
        self.note_files(&*staging.tier);
        for blob in batch {
            let outcome = match &committed {
                Ok(()) => Outcome::Staged,
                Err(e) => Outcome::Failed(e.clone()),
            };
            let _ = blob.done.send(outcome);
        }
    }

    /// Reserves one range for `unique`, writes the blobs in it one after another, and
    /// syncs it: the synced range, and each blob's offset in its block.
    fn write(
        &self,
        tier: &dyn WriteTier,
        unique: &BTreeMap<Digest, &Bytes>,
    ) -> Result<(Synced, Vec<(Digest, u64)>), TierError> {
        let total = unique.values().map(|b| b.len() as u64).sum();
        let reservation = tier.reserve_group(total, Instant::now())?;
        self.note_files(tier);
        let mut at = 0;
        let mut listed = Vec::with_capacity(unique.len());
        for (digest, bytes) in unique {
            tier.write(&reservation, at, bytes)?;
            listed.push((*digest, at));
            at += bytes.len() as u64;
        }
        let synced = tier.sync(reservation).map_err(|unsynced| unsynced.error)?;
        let base = synced.offset();
        let listed = listed.into_iter().map(|(d, at)| (d, base + at)).collect();
        Ok((synced, listed))
    }

    fn send_to_store(&self, batch: Vec<Whole>) {
        self.metrics().write_pool_direct.add(batch.len() as u64);
        for blob in batch {
            let _ = blob.done.send(Outcome::NoRoom);
        }
    }

    /// One `PutStaged` on the cache's runtime, waited for.
    fn put_staged(
        &self,
        synced: Synced,
        blobs: Vec<(Digest, u64)>,
        seal: bool,
    ) -> Result<(), CacheError> {
        let (done, answer) = oneshot::channel();
        let (writer, staging) = (Arc::clone(&self.writer), Arc::clone(&self.staging));
        self.runtime.spawn(async move {
            let put = writer.put_staged(&staging, synced, blobs, seal).await;
            let _ = done.send(put);
        });
        // The runtime stopped before the commit was answered: nothing is committed.
        let stopped = CacheError::Internal("the runtime stopped before a commit".to_owned());
        answer.blocking_recv().unwrap_or(Err(stopped))
    }

    /// Streams one blob into the tier as its chunks arrive, holding this writer until it
    /// is committed or abandoned.
    fn stream(&self, job: StreamJob) {
        let StreamJob {
            size,
            accept,
            chunks,
            commit,
        } = job;
        let staging = &self.staging;
        let tier = &*staging.tier;
        let reserved = match tier.reserve_group(size, Instant::now()) {
            // Too large to share a block: a block of its own, sealed by its commit.
            Err(TierError::TooLarge { .. }) => tier.reserve_upload(size).map(|r| (r, true)),
            other => other.map(|r| (r, false)),
        };
        self.note_files(tier);
        let (reservation, own) = match reserved {
            Ok(reserved) => reserved,
            Err(e) => {
                staging.counters.direct(&e);
                self.metrics().write_pool_direct.add(1);
                let _ = accept.send(false);
                return;
            }
        };
        let _ = accept.send(true);
        self.fill(reservation, own, chunks, commit);
        self.note_files(tier);
    }

    /// Writes the chunks into `reservation`, then, once the sender commits, syncs and
    /// commits the blob. Abandoned (the sender dropped the chunks or the commit), the
    /// reservation is dropped: answered as failed.
    fn fill(
        &self,
        reservation: Reservation,
        own: bool,
        chunks: Receiver<Bytes>,
        commit: oneshot::Receiver<(Digest, oneshot::Sender<Result<(), CacheError>>)>,
    ) {
        let mut at = 0;
        for chunk in chunks {
            if let Err(e) = self.staging.tier.write(&reservation, at, &chunk) {
                tracing::warn!(error = %e, "the write pool could not write a streamed blob");
                return;
            }
            at += chunk.len() as u64;
        }
        let Ok((digest, done)) = commit.blocking_recv() else {
            return;
        };
        // `RecordStream::finish` checked that the bytes sent are as many as the digest names.
        let answer = self.finish(reservation, own, digest);
        let _ = done.send(answer);
    }

    fn finish(
        &self,
        reservation: Reservation,
        own: bool,
        digest: Digest,
    ) -> Result<(), CacheError> {
        let synced = self
            .staging
            .tier
            .sync(reservation)
            .map_err(|unsynced| CacheError::from(unsynced.error))?;
        let at = synced.offset();
        self.put_staged(synced, vec![(digest, at)], own)
    }

    fn note_files(&self, tier: &dyn WriteTier) {
        self.metrics().open_write_fds.set(tier.open_write_files());
    }
}

/// A blob being received for the pool: held in memory, or streaming to a writer.
pub(crate) struct PoolWrite {
    inner: Inner,
    size: u64,
}

enum Inner {
    Buffered {
        bytes: Vec<u8>,
        jobs: Jobs,
    },
    Streaming {
        chunks: SyncSender<Bytes>,
        /// The writer's answer, until the first chunk waited for it.
        accept: Option<oneshot::Receiver<bool>>,
        commit: oneshot::Sender<(Digest, oneshot::Sender<Result<(), CacheError>>)>,
    },
}

/// Why [`PoolWrite::append`] did not take a chunk.
pub(crate) enum Refused {
    /// The tier has no room for the blob: nothing was written, and the caller sends it
    /// to the store.
    NoRoom,
    /// The append failed.
    Failed(CacheError),
}

impl PoolWrite {
    /// A blob of `size` bytes for the pool; `None` for a size of 0, or when the pool's
    /// queue is full (counted as sent to the store).
    pub(super) fn start(pool: &WritePool, metrics: &CacheMetrics, size: u64) -> Option<Self> {
        if size == 0 {
            return None;
        }
        if size <= BUFFERED_BLOB_BYTES {
            if pool.full(metrics) {
                metrics.write_pool_direct.add(1);
                return None;
            }
            let jobs = pool.jobs.clone();
            let bytes = Vec::new();
            return Some(Self {
                inner: Inner::Buffered { bytes, jobs },
                size,
            });
        }
        let (chunks, queued_chunks) = mpsc::sync_channel(STREAM_CHUNKS_IN_FLIGHT);
        let (accept_tx, accept_rx) = oneshot::channel();
        let (commit_tx, commit_rx) = oneshot::channel();
        let job = Job::Stream(StreamJob {
            size,
            accept: accept_tx,
            chunks: queued_chunks,
            commit: commit_rx,
        });
        // Counted before the send: a writer may take the job at once and lower it.
        metrics.write_pool_queue.add_one();
        if pool.jobs.0.try_send(job).is_err() {
            metrics.write_pool_queue.sub_one();
            metrics.write_pool_direct.add(1);
            return None;
        }
        Some(Self {
            inner: Inner::Streaming {
                chunks,
                accept: Some(accept_rx),
                commit: commit_tx,
            },
            size,
        })
    }

    /// The bytes the blob was made for.
    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    /// Takes `chunk`. A streaming blob first waits for its writer to take it. Blocking:
    /// call it off the async runtime.
    pub(crate) fn append(&mut self, chunk: &[u8]) -> Result<(), Refused> {
        match &mut self.inner {
            Inner::Buffered { bytes, .. } => {
                bytes.extend_from_slice(chunk);
                Ok(())
            }
            Inner::Streaming { chunks, accept, .. } => {
                let stopped = || Refused::Failed(CacheError::Invalid(STOPPED.to_owned()));
                if let Some(accept) = accept.take() {
                    // A writer that went away without answering took no room either.
                    if accept.blocking_recv() != Ok(true) {
                        return Err(Refused::NoRoom);
                    }
                }
                chunks
                    .send(Bytes::copy_from_slice(chunk))
                    .map_err(|_| stopped())
            }
        }
    }

    /// Ends the bytes: what [`Cache::pool_commit`] commits.
    pub(crate) fn end(self) -> PoolCommit {
        match self.inner {
            Inner::Buffered { bytes, jobs } => PoolCommit::Whole {
                bytes: Bytes::from(bytes),
                jobs,
            },
            // The chunks end with their sender.
            Inner::Streaming { commit, .. } => PoolCommit::Stream { commit },
        }
    }
}

/// A blob of the pool whose bytes are all in and checked, not yet committed.
pub(crate) enum PoolCommit {
    Whole {
        bytes: Bytes,
        jobs: Jobs,
    },
    Stream {
        commit: oneshot::Sender<(Digest, oneshot::Sender<Result<(), CacheError>>)>,
    },
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// The pool of this cache's tier, started with the first blob; `None` without a tier
    /// or a runtime, or if no writer thread could be started.
    pub(super) fn write_pool(&self) -> Option<&WritePool> {
        let staging = self.writer.tier.get()?;
        self.pool
            .get_or_init(|| {
                let runtime = self.runtime.clone()?;
                WritePool::start(Arc::clone(&self.writer), staging, runtime, self.pool_config)
            })
            .as_ref()
    }

    /// A blob of `size` bytes for the pool, if there is one and it has room.
    pub(super) fn pool_open(&self, size: u64) -> Option<PoolWrite> {
        let pool = self.write_pool()?;
        // A blob the tier's budget cannot hold, as of now, goes to the store without being
        // received into memory first (the writer would find no room for it either).
        let staging = self.writer.tier.get()?;
        let usage = staging.tier.usage();
        if size > 0 && usage.used.saturating_add(size) > usage.budget {
            let full = TierError::Full {
                want: size,
                used: usage.used,
                budget: usage.budget,
            };
            staging.counters.direct(&full);
            self.writer.metrics.write_pool_direct.add(1);
            return None;
        }
        PoolWrite::start(pool, &self.writer.metrics, size)
    }

    /// Makes the blob `digest` durable and present: through a writer, which syncs it and
    /// commits it (alone or with others), or, if the queue is full or the tier does not
    /// take it, straight to the store. Answers once it is present.
    ///
    /// # Errors
    /// The sync or the commit failed, or the store did; nothing is stored.
    pub(super) async fn pool_commit(
        &self,
        commit: PoolCommit,
        digest: Digest,
    ) -> Result<(), CacheError> {
        match commit {
            PoolCommit::Stream { commit } => {
                let (done, answer) = oneshot::channel();
                // A writer that is gone drops `done` with the message, and the answer fails.
                let _ = commit.send((digest, done));
                let stopped = CacheError::Invalid(STOPPED.to_owned());
                answer.await.unwrap_or(Err(stopped))
            }
            PoolCommit::Whole { bytes, jobs } => {
                let (done, answer) = oneshot::channel();
                let job = Job::Whole(Whole {
                    digest,
                    bytes: bytes.clone(),
                    done,
                });
                // Counted before the send: a writer may take the job at once and lower it.
                self.writer.metrics.write_pool_queue.add_one();
                if jobs.0.try_send(job).is_err() {
                    self.writer.metrics.write_pool_queue.sub_one();
                    self.writer.metrics.write_pool_direct.add(1);
                    return self.store_whole(digest, bytes).await;
                }
                // A writer that went away without answering stored nothing: the bytes are
                // still here, and go to the store.
                match answer.await.unwrap_or(Outcome::NoRoom) {
                    Outcome::Staged => Ok(()),
                    Outcome::NoRoom => self.store_whole(digest, bytes).await,
                    Outcome::Failed(e) => Err(e),
                }
            }
        }
    }

    /// The blob `digest` held whole, straight to the object store.
    async fn store_whole(&self, digest: Digest, bytes: Bytes) -> Result<(), CacheError> {
        let body = futures::stream::once(async move { Ok(bytes) }).boxed();
        self.store_streamed(digest, body).await
    }
}
