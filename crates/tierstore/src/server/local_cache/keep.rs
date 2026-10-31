//! The create-only put of a cache that writes through, and the copy of the object it keeps.
//!
//! A put under a `cache = true` policy is copied into `<path>/.stage/` as its body streams to
//! the upstream, and once the upstream has it the copy is put in the cache before the put is
//! answered, so the node reads what it wrote without a fill:
//!
//! 1. the upstream's put acknowledges the object;
//! 2. a HEAD of the key gives the tag and the time the upstream gave it (the put answers
//!    nothing, and the tag of an object made of parts is not the MD5 of its bytes, so it cannot
//!    be worked out here);
//! 3. the sidecar is renamed into place, the data file is linked, and the object is put in the
//!    index (the steps of a fill, shared with it: `reserve`, `Staged` and `publish`);
//! 4. the put is answered.
//!
//! Steps 2 and 3 run on a task of their own that the put waits for, so a request that is
//! dropped after the acknowledgement (a client that goes away) does not stop the keep half way:
//! the task ends with the object in the index and its files in place, or with no file.
//!
//! The put waits for the task for `keep_wait` (5 s unless set) and no longer. Past it the put is
//! answered 200, counted in `keeps_expired_total`, and the task goes on alone: it ends with the
//! object kept or abandoned, and in either case no file, permit or reservation is left. This is
//! safe because the upstream holds the object before the wait begins: a read that misses before
//! the keep ends is a fill or a read through, which costs the trip to the upstream and nothing
//! else. What past `keep_wait` stops being is the promise that the node reads what it wrote
//! without the trip, which was best effort already. The eviction pass a kept object may call for
//! runs on a task of its own, after the keep, and the put never waits for it.
//!
//! The copy is best effort. A put that finds every keep running, no room in the budget, a stage
//! directory that cannot be written, a disk that falls behind the body, a HEAD that fails, or a
//! key that a fill is running for, is answered as if the cache kept nothing: the stage file is
//! removed, the skip is counted in `write_keep_skipped_total{reason}` and the put succeeds. The
//! keeps have a limit of their own (`max_concurrent_keeps`), apart from the fills.
//!
//! - The body is written to the stage file by a task of its own, through a queue of at most
//!   [`KEEP_QUEUE`] chunks and [`KEEP_QUEUE_BYTES`] bytes (the bytes bound the memory, whatever
//!   the size of the chunks the front delivers). The body never waits for the disk: a chunk that
//!   finds the queue full drops the copy (`slow`), so a hung disk costs the node the copy and
//!   the upstream nothing.
//! - The keep as a whole has `keep_deadline` (60 s unless set; not below `keep_wait`) from the
//!   acknowledgement. A disk that hangs under a body too small to fill the queue, or an upstream
//!   that does not answer the HEAD, would otherwise hold the keep for ever; at the deadline the
//!   keep is abandoned: its stage file is removed, its permit and room are given back, its
//!   place in the table of the fills is freed, and it is counted as `deadline`. A link that is
//!   under way at the deadline cannot be called back; it is undone as soon as it ends. Two
//!   residuals of that: a link whose thread never returns (a disk that hangs for good) leaks the
//!   staged file and the `Arc` of the cache that the cleanup holds, until the process ends; and
//!   the cleanup's `drop_entry` may remove the entry of a fill of the same key that a reader
//!   started and finished after the abandonment, which costs that reader's next read a trip to
//!   the upstream (a lost entry, never stale data: the cleanup removes files, it adds none).
//! - A key named by its content (`cas/l/<sha256>`) is hashed as the body passes, whether or not
//!   it is copied. The check is made when the declared length has passed, in place of the chunk
//!   that completes the body, or when the body ends: a body that does not hash to the name ends
//!   the stream with [`Error::BadDigest`], so that the upstream, which stores nothing of a body
//!   that fails, has no object, and nothing is kept. An upstream that acknowledges a put without
//!   having read the body to its end is not trusted: a put of such a key is [`Error::BadDigest`]
//!   (the object is at the upstream, unchecked, which only an upstream that breaks the contract
//!   of [`put_stream`](crate::Backend::put_stream) can cause), and any other put is not kept.
//! - A body that is cut ends the put with its error and leaves nothing in either place.
//! - A body of no declared length cannot be reserved for and is not kept (counted as `budget`).
//!   The front asks for the length whenever the upstream does.
//! - A delete that comes while the object is being kept leaves nothing. The keep takes the key's
//!   place in the table of the fills that run before it asks for the HEAD, so a delete that
//!   finishes after the HEAD marks it (see [`forget`](Inner::forget)) and the keep removes its
//!   own entry, whenever the delete comes before the check that follows the insert; a delete
//!   that finishes before the HEAD makes the HEAD fail. Readers of the key never wait for the
//!   keep (unlike a fill, which they share): they are answered from the upstream, at once.
//! - A multipart upload is not kept: the first read of the object fills it.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _, stream};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::{Error, PartBody};

use super::fill::{Filled, Flight, Pending, Published, Reserved, Staged};
use super::layout::{Name, Sidecar};
use super::metrics::{SkippedKeep, bump};
use super::{Inner, locked};

/// The chunks of a body that may wait for the disk.
pub(super) const KEEP_QUEUE: usize = 16;

/// The bytes of a body that may wait for the disk. A chunk that would take the queue past this
/// (the first one is always taken) drops the copy. This, and not the number of chunks, bounds the
/// memory of the copy of one put, whatever the size of the chunks the front delivers; the keeps
/// that run at once hold at most `max_concurrent_keeps` times this, 32 MiB by default.
pub(super) const KEEP_QUEUE_BYTES: u64 = 4 << 20;

/// The task that writes the chunks of a body to the stage file, and the way to it.
pub(super) struct Writer {
    /// The way to the task; none once the body has ended.
    chunks: Option<mpsc::Sender<Bytes>>,
    /// The bytes sent to the task and not yet written.
    queued: Arc<AtomicU64>,
    /// The task, which ends with the stage file when the way to it is closed. Aborted if the
    /// copy is dropped before that, which removes the stage file with it.
    task: Option<JoinHandle<io::Result<Staged>>>,
}

impl Writer {
    /// Starts the task that writes what it is sent to `staged`.
    pub(super) fn start(inner: &Arc<Inner>, staged: Staged) -> Self {
        let (chunks, queue) = mpsc::channel(KEEP_QUEUE);
        let queued = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(write_chunks(
            Arc::clone(inner),
            staged,
            queue,
            Arc::clone(&queued),
        ));
        Self {
            chunks: Some(chunks),
            queued,
            task: Some(task),
        }
    }

    /// Queues a chunk without waiting.
    pub(super) fn send(&self, chunk: Bytes) -> Result<(), SkippedKeep> {
        let Some(chunks) = &self.chunks else {
            return Err(SkippedKeep::Io);
        };
        let size = chunk.len() as u64;
        let before = self.queued.fetch_add(size, Ordering::SeqCst);
        if before != 0 && before + size > KEEP_QUEUE_BYTES {
            let _ = self.queued.fetch_sub(size, Ordering::SeqCst);
            return Err(SkippedKeep::Slow);
        }
        chunks.try_send(chunk).map_err(|error| {
            let _ = self.queued.fetch_sub(size, Ordering::SeqCst);
            match error {
                TrySendError::Full(_) => SkippedKeep::Slow,
                TrySendError::Closed(_) => SkippedKeep::Io,
            }
        })
    }

    /// Waits for the task to write what was queued, and gives the stage file. If this future is
    /// dropped before it ends, the task is still the writer's and is aborted with it.
    pub(super) async fn finish(&mut self) -> io::Result<Staged> {
        drop(self.chunks.take());
        let Some(task) = self.task.as_mut() else {
            return Err(io::Error::other("the writer was finished already"));
        };
        let ended = task.await;
        self.task = None;
        ended.map_err(io::Error::other)?
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Writes the chunks of `queue` to `staged` until the queue is closed. A write that fails ends
/// the task with its error, and the stage file with it.
async fn write_chunks(
    inner: Arc<Inner>,
    mut staged: Staged,
    mut queue: mpsc::Receiver<Bytes>,
    queued: Arc<AtomicU64>,
) -> io::Result<Staged> {
    while let Some(chunk) = queue.recv().await {
        let written = inner.disk.write_stage(&mut staged.file, &chunk).await;
        let _ = queued.fetch_sub(chunk.len() as u64, Ordering::SeqCst);
        written?;
    }
    Ok(staged)
}

/// The copy of a body that is being made: the writer of the stage file, the room reserved for
/// it and the keep permit, all given back when it is dropped.
pub(super) struct Stage {
    writer: Writer,
    _reservation: Reserved,
    _permit: OwnedSemaphorePermit,
}

/// What a put knows of its body while the body passes.
pub(super) struct Keeping {
    /// The copy, while it is being made and whole so far.
    stage: Option<Stage>,
    /// Why there is no copy, if there is none.
    lost: Option<SkippedKeep>,
    /// The hash of the bytes so far; none if nothing needs it, or once it is worked out.
    hasher: Option<Sha256>,
    /// The SHA-256 the bytes must have, for a key named by its content.
    wanted: Option<String>,
    /// The bytes that have passed.
    seen: u64,
    /// The length the put declared.
    len: Option<u64>,
    /// Whether the body has been read to its end and checked.
    finished: bool,
    /// The hex SHA-256 of the whole body, once it is finished.
    digest: String,
}

impl Keeping {
    pub(super) fn new(
        wanted: Option<String>,
        len: Option<u64>,
        stage: Result<Stage, SkippedKeep>,
    ) -> Self {
        let (stage, lost) = match stage {
            Ok(stage) => (Some(stage), None),
            Err(why) => (None, Some(why)),
        };
        let hashing = stage.is_some() || wanted.is_some();
        Self {
            stage,
            lost,
            hasher: hashing.then(Sha256::new),
            wanted,
            seen: 0,
            len,
            finished: false,
            digest: String::new(),
        }
    }

    /// Takes a chunk of the body: hashes it and queues it for the stage file, without waiting.
    /// A copy that cannot be queued is dropped, not the put. The chunk that completes the
    /// declared length finishes the body (see [`finish`](Self::finish)), so that the check
    /// is made before the upstream has the last byte.
    fn take(&mut self, key: &str, chunk: &Bytes) -> Result<(), Error> {
        self.seen += chunk.len() as u64;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(chunk);
        }
        if let Some(stage) = &self.stage
            && let Err(why) = stage.writer.send(chunk.clone())
        {
            self.stage = None;
            self.lost = Some(why);
        }
        if self.len.is_some_and(|len| self.seen >= len) {
            return self.finish(key);
        }
        Ok(())
    }

    /// The end of the body, once: works out the hash. A key named by its content whose bytes
    /// hash to another name is an error, and nothing is kept.
    fn finish(&mut self, key: &str) -> Result<(), Error> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let Some(hasher) = self.hasher.take() else {
            return Ok(());
        };
        self.digest = hex::encode(hasher.finalize());
        if self
            .wanted
            .as_ref()
            .is_some_and(|wanted| *wanted != self.digest)
        {
            self.stage = None;
            return Err(Error::BadDigest(key.to_owned()));
        }
        Ok(())
    }
}

/// The body, passed on as it is, with each chunk given to `keeping` on the way.
pub(super) fn tee<'a>(key: &'a str, body: PartBody<'a>, keeping: &'a mut Keeping) -> PartBody<'a> {
    let state = (body, keeping, false);
    stream::unfold(state, move |(mut body, keeping, done)| async move {
        if done {
            return None;
        }
        match body.next().await {
            Some(Ok(chunk)) => match keeping.take(key, &chunk) {
                Ok(()) => Some((Ok(chunk), (body, keeping, false))),
                Err(error) => Some((Err(error), (body, keeping, true))),
            },
            Some(Err(error)) => Some((Err(error), (body, keeping, true))),
            None => match keeping.finish(key) {
                Ok(()) => None,
                Err(error) => Some((Err(error), (body, keeping, true))),
            },
        }
    })
    // `unfold` panics if it is polled after its end; a backend that polls a body again after
    // the end must get the end again, not a panic in the task of a request.
    .fuse()
    .boxed()
}

impl Inner {
    /// A create-only put of a body that arrives as chunks.
    pub(super) async fn put_through(
        self: &Arc<Self>,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.writable(key)?;
        match self.upstream.head(key).await {
            Ok(_) => {
                bump(&self.counters.write_exists);
                return Err(Error::AlreadyExists(key.to_owned()));
            }
            Err(Error::NotFound(_)) => {}
            Err(other) => return Err(other),
        }
        let name = self.cacheable(key);
        let mut keeping = match &name {
            Some(name) => {
                let stage = self.open_stage(len).await;
                Some(Keeping::new(name.content.clone(), len, stage))
            }
            None => None,
        };
        let sent = match &mut keeping {
            Some(keeping) => {
                let teed = tee(key, body, keeping);
                self.upstream.put_stream(key, len, teed).await
            }
            None => self.upstream.put_stream(key, len, body).await,
        };
        match sent {
            Ok(()) => {}
            Err(Error::AlreadyExists(exists)) => {
                bump(&self.counters.write_exists);
                return Err(Error::AlreadyExists(exists));
            }
            Err(other) => return Err(other),
        }
        bump(&self.counters.writes_through);
        if let (Some(name), Some(mut keeping)) = (name, keeping) {
            if !keeping.finished {
                // The upstream took the put without reading the body to its end.
                if keeping.wanted.is_some() {
                    return Err(Error::BadDigest(key.to_owned()));
                }
                keeping.stage = None;
                keeping.lost = Some(SkippedKeep::Head);
            }
            // On a task of its own, so that a request that is dropped from here on does not stop
            // the keep half way (a blocking publish would go on with nobody to index it).
            let (inner, owned) = (Arc::clone(self), key.to_owned());
            let mut kept = tokio::spawn(async move { inner.keep(&owned, &name, keeping).await });
            // The put waits for its keep, but not for ever: past `keep_wait` it is answered and
            // the keep goes on alone. The object is at the upstream, so a read that misses
            // before the keep ends costs the trip and nothing else. A keep that panicked is a
            // keep that did not happen; the put has succeeded.
            if tokio::time::timeout(self.config.keep_wait, &mut kept)
                .await
                .is_err()
            {
                bump(&self.counters.keeps_expired);
            }
        }
        Ok(())
    }

    /// The copy for a body of `len` bytes: a keep permit, room for the bytes and a stage file
    /// with its writer.
    async fn open_stage(self: &Arc<Self>, len: Option<u64>) -> Result<Stage, SkippedKeep> {
        let Ok(permit) = Arc::clone(&self.keeps).try_acquire_owned() else {
            return Err(SkippedKeep::Busy);
        };
        let size = len.ok_or(SkippedKeep::Budget)?;
        let reservation = self.reserve(size).ok_or(SkippedKeep::Budget)?;
        let staged = Staged::create(self).await.map_err(|_| SkippedKeep::Io)?;
        Ok(Stage {
            writer: Writer::start(self, staged),
            _reservation: reservation,
            _permit: permit,
        })
    }

    /// Puts the copy of an object the upstream has taken in the cache, or counts why not. Never
    /// an error: the put has succeeded.
    async fn keep(self: &Arc<Self>, key: &str, name: &Name, mut keeping: Keeping) {
        let Some(stage) = keeping.stage.take() else {
            self.counters
                .keep_skipped(keeping.lost.unwrap_or(SkippedKeep::Io));
            // The key was not there, so an entry for it is of an object that was deleted behind
            // the cache's back.
            self.forget(key);
            return;
        };
        let Some(flight) = self.enter(name) else {
            self.counters.keep_skipped(SkippedKeep::Busy);
            self.forget(key);
            return;
        };
        let written = (keeping.seen, keeping.digest);
        if let Err(why) = self.publish_put(key, name, stage, written).await {
            self.counters.keep_skipped(why);
            self.forget(key);
        }
        drop(flight);
    }

    /// Takes the place of a fill of the key in the table of the fills that run, so that a
    /// delete marks the keep. None if a fill is running for the key.
    ///
    /// A reader that finds the place taken does not wait: the entry stands for a keep, which a
    /// reader has no reason to wait for (the keep may be held up by a disk that hangs, and the
    /// object is at the upstream), so it is a fill that is already over, with no object: the
    /// reader is answered from the upstream and starts no fill.
    fn enter(self: &Arc<Self>, name: &Name) -> Option<Flight> {
        let over: BoxFuture<'static, Result<Filled, Error>> =
            futures::future::ready(Ok(Filled::Skipped)).boxed();
        let pending: Pending = over.shared();
        {
            let mut flights = locked(&self.flights);
            if flights.contains_key(&name.hex) {
                return None;
            }
            drop(flights.insert(name.hex.clone(), pending));
        }
        Some(Flight {
            inner: Arc::clone(self),
            hex: name.hex.clone(),
        })
    }

    /// `step`, if it ends before `deadline`; else [`SkippedKeep::Deadline`], with `step` dropped.
    async fn before<T>(deadline: Instant, step: impl Future<Output = T>) -> Result<T, SkippedKeep> {
        tokio::time::timeout_at(deadline, step)
            .await
            .map_err(|_| SkippedKeep::Deadline)
    }

    /// Runs the hook a test set to act just before a kept object is put in the index.
    #[cfg(test)]
    fn run_before_insert(&self) {
        if let Some(hook) = locked(&self.before_insert).as_ref() {
            hook();
        }
    }

    /// Steps 2 and 3: asks the upstream what it gave the object, and puts the copy in the cache.
    async fn publish_put(
        self: &Arc<Self>,
        key: &str,
        name: &Name,
        mut stage: Stage,
        written: (u64, String),
    ) -> Result<(), SkippedKeep> {
        let (size, digest) = written;
        let deadline = Instant::now() + self.config.keep_deadline;
        let meta = match Self::before(deadline, self.upstream.head(key)).await? {
            Ok(meta) if meta.size == size => meta,
            _ => return Err(SkippedKeep::Head),
        };
        let ended = Self::before(deadline, stage.writer.finish()).await?;
        let mut staged = ended.map_err(|_| SkippedKeep::Io)?;
        let synced = Self::before(deadline, staged.finish()).await?;
        synced.map_err(|_| SkippedKeep::Io)?;
        // An entry for the key is of an object that is gone: it must not keep the new file's
        // place.
        self.drop_entry(&name.hex);
        let sidecar = Sidecar {
            key: key.to_owned(),
            size,
            sha256: digest,
            tag: meta.tag.clone(),
            modified: meta.modified,
        };
        let generation = self.generation();
        let mut publishing = self.begin_commit(&staged, name, &sidecar, generation);
        match tokio::time::timeout_at(deadline, &mut publishing).await {
            Ok(joined) => {
                let published = joined
                    .expect("a publish does not panic")
                    .map_err(|_| SkippedKeep::Io)?;
                if published == Published::Lost {
                    return Err(SkippedKeep::Io);
                }
            }
            Err(_) => {
                // The link cannot be called back: it is left to end on its own, and then undone,
                // so that it leaves no file that nothing indexes.
                let inner = Arc::clone(self);
                let hex = name.hex.clone();
                drop(tokio::spawn(async move {
                    let _ = publishing.await;
                    inner.drop_entry(&hex);
                    drop(staged);
                }));
                return Err(SkippedKeep::Deadline);
            }
        };
        #[cfg(test)]
        self.run_before_insert();
        locked(&self.index).insert(&name.hex, size, meta.tag, meta.modified);
        // A delete that finished after the HEAD marked this keep (see `forget`), and may have
        // dropped nothing because the entry was not in yet: the object is gone from the
        // upstream. The mark is looked for after the entry is in the index.
        if self.is_invalidated(&name.hex) {
            self.drop_entry(&name.hex);
            return Err(SkippedKeep::Deleted);
        }
        drop(stage);
        // The pass is not the put's to wait for. A pass that is running makes another return
        // at once (see `evict_pass`), so passes started here cannot run twice at the same time.
        let inner = Arc::clone(self);
        drop(tokio::spawn(async move { inner.relieve().await }));
        Ok(())
    }
}
