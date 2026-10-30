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
//! The copy is best effort. A put that finds every keep running, no room in the budget, a stage
//! directory that cannot be written, a disk that falls behind the body, a HEAD that fails, or a
//! key that a fill is running for, is answered as if the cache kept nothing: the stage file is
//! removed, the skip is counted in `write_keep_skipped_total{reason}` and the put succeeds. The
//! keeps have a limit of their own (`max_concurrent_keeps`), apart from the fills.
//!
//! - The body is written to the stage file by a task of its own, through a queue of
//!   [`KEEP_QUEUE`] chunks. The body never waits for the disk: a chunk that finds the queue full
//!   drops the copy (`slow`), so a hung disk costs the node the copy and the upstream nothing.
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
//!   that finishes before the HEAD makes the HEAD fail. Readers of the key wait for the keep as
//!   they would for a fill.
//! - A multipart upload is not kept: the first read of the object fills it.

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _, stream};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{Error, PartBody};

use super::fill::{Filled, Flight, Pending, Reserved, Staged};
use super::layout::{Name, Sidecar};
use super::metrics::{SkippedKeep, bump};
use super::{Inner, locked};

/// The chunks of a body that may wait for the disk. A body of chunks of 64 KiB holds 1 MiB of
/// memory at most in the queue of one put; a chunk that finds the queue full drops the copy.
pub(super) const KEEP_QUEUE: usize = 16;

/// The task that writes the chunks of a body to the stage file, and the way to it.
struct Writer {
    /// The way to the task; none once the body has ended.
    chunks: Option<mpsc::Sender<Bytes>>,
    /// The task, which ends with the stage file when the way to it is closed. Aborted if the
    /// copy is dropped before that, which removes the stage file with it.
    task: Option<JoinHandle<io::Result<Staged>>>,
}

impl Writer {
    /// Starts the task that writes what it is sent to `staged`.
    fn start(inner: &Arc<Inner>, staged: Staged) -> Self {
        let (chunks, queue) = mpsc::channel(KEEP_QUEUE);
        let task = tokio::spawn(write_chunks(Arc::clone(inner), staged, queue));
        Self {
            chunks: Some(chunks),
            task: Some(task),
        }
    }

    /// Queues a chunk without waiting.
    fn send(&self, chunk: Bytes) -> Result<(), SkippedKeep> {
        let Some(chunks) = &self.chunks else {
            return Err(SkippedKeep::Io);
        };
        chunks.try_send(chunk).map_err(|error| match error {
            TrySendError::Full(_) => SkippedKeep::Slow,
            TrySendError::Closed(_) => SkippedKeep::Io,
        })
    }

    /// Waits for the task to write what was queued, and gives the stage file.
    async fn finish(&mut self) -> io::Result<Staged> {
        drop(self.chunks.take());
        let Some(task) = self.task.take() else {
            return Err(io::Error::other("the writer was finished already"));
        };
        task.await.map_err(io::Error::other)?
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
) -> io::Result<Staged> {
    while let Some(chunk) = queue.recv().await {
        inner.disk.write_stage(&mut staged.file, &chunk).await?;
    }
    Ok(staged)
}

/// The copy of a body that is being made: the writer of the stage file, the room reserved for
/// it and the keep permit, all given back when it is dropped.
struct Stage {
    writer: Writer,
    _reservation: Reserved,
    _permit: OwnedSemaphorePermit,
}

/// What a put knows of its body while the body passes.
struct Keeping {
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
    fn new(wanted: Option<String>, len: Option<u64>, stage: Result<Stage, SkippedKeep>) -> Self {
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
fn tee<'a>(key: &'a str, body: PartBody<'a>, keeping: &'a mut Keeping) -> PartBody<'a> {
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
            let kept = tokio::spawn(async move { inner.keep(&owned, &name, keeping).await });
            // A keep that panicked is a keep that did not happen; the put has succeeded.
            let _ = kept.await;
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
        let Some((flight, sender)) = self.enter(name) else {
            self.counters.keep_skipped(SkippedKeep::Busy);
            self.forget(key);
            return;
        };
        let written = (keeping.seen, keeping.digest);
        let kept = self.publish_put(key, name, stage, written).await;
        let filled = match kept {
            Ok(()) => Filled::Kept,
            Err(why) => {
                self.counters.keep_skipped(why);
                self.forget(key);
                Filled::Skipped
            }
        };
        // The readers that waited for the keep find the entry, or go to the upstream.
        let _ = sender.send(filled);
        drop(flight);
    }

    /// Takes the place of a fill of the key in the table of the fills that run, so that a
    /// reader waits for the keep and a delete marks it. None if a fill is running for the key.
    fn enter(self: &Arc<Self>, name: &Name) -> Option<(Flight, oneshot::Sender<Filled>)> {
        let (sender, receiver) = oneshot::channel();
        let waiting: BoxFuture<'static, Result<Filled, Error>> =
            async move { Ok(receiver.await.unwrap_or(Filled::Skipped)) }.boxed();
        let pending: Pending = waiting.shared();
        {
            let mut flights = locked(&self.flights);
            if flights.contains_key(&name.hex) {
                return None;
            }
            drop(flights.insert(name.hex.clone(), pending));
        }
        let flight = Flight {
            inner: Arc::clone(self),
            hex: name.hex.clone(),
        };
        Some((flight, sender))
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
        let meta = match self.upstream.head(key).await {
            Ok(meta) if meta.size == size => meta,
            _ => return Err(SkippedKeep::Head),
        };
        let mut staged = stage.writer.finish().await.map_err(|_| SkippedKeep::Io)?;
        staged.finish().await.map_err(|_| SkippedKeep::Io)?;
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
        let committed = self.commit(&staged, name, &sidecar).await;
        committed.map_err(|_| SkippedKeep::Io)?;
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
        self.relieve().await;
        Ok(())
    }
}
