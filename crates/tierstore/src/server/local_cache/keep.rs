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
//! The copy is best effort. A put that finds every keep running, no room in the budget, a stage
//! directory that cannot be written, a HEAD that fails, or a key that a fill is running for, is
//! answered as if the cache kept nothing: the stage file is removed, the skip is counted in
//! `write_keep_skipped_total{reason}` and the put succeeds. The keeps have a limit of their own
//! (`max_concurrent_keeps`), apart from the fills.
//!
//! - A key named by its content (`cas/l/<sha256>`) is hashed as the body passes, whether or not
//!   it is copied. A body that does not hash to the name ends the stream with an error, so that
//!   the upstream, which stores nothing of a body that fails, has no object, and nothing is
//!   kept.
//! - A body that is cut ends the put with its error and leaves nothing in either place.
//! - A body of no declared length cannot be reserved for and is not kept (counted as `budget`).
//!   The front asks for the length whenever the upstream does.
//! - A delete that comes while the object is being kept leaves nothing. The keep takes the key's
//!   place in the table of the fills that run before it asks for the HEAD, so a delete that
//!   finishes after the HEAD marks it (see [`forget`](Inner::forget)) and the keep removes its
//!   own entry; a delete that finishes before the HEAD makes the HEAD fail. Readers of the key
//!   wait for the keep as they would for a fill.
//! - A multipart upload is not kept: the first read of the object fills it.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _, stream};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, oneshot};

use crate::{Error, PartBody};

use super::fill::{Filled, Flight, Pending, Reserved, Staged};
use super::layout::{Name, Sidecar};
use super::metrics::{SkippedKeep, bump};
use super::{Inner, locked};

/// The copy of a body that is being made: the stage file, the room reserved for it and the keep
/// permit, all given back when it is dropped.
struct Stage {
    staged: Staged,
    _reservation: Reserved,
    _permit: OwnedSemaphorePermit,
}

/// What a put knows of its body while the body passes.
struct Keeping {
    /// The copy, while it is being made and whole so far.
    stage: Option<Stage>,
    /// Why there is no copy, if there is none.
    lost: Option<SkippedKeep>,
    /// The hash of the bytes so far; none if nothing needs it.
    hasher: Option<Sha256>,
    /// The SHA-256 the bytes must have, for a key named by its content.
    wanted: Option<String>,
    /// The bytes that have passed.
    seen: u64,
    /// The hex SHA-256 of the whole body, once it has ended.
    digest: String,
}

impl Keeping {
    fn new(wanted: Option<String>, stage: Result<Stage, SkippedKeep>) -> Self {
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
            digest: String::new(),
        }
    }

    /// Takes a chunk of the body: hashes it and writes it to the stage file. A write that fails
    /// ends the copy, not the put.
    async fn take(&mut self, chunk: &Bytes) {
        self.seen += chunk.len() as u64;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(chunk);
        }
        let Some(stage) = &mut self.stage else {
            return;
        };
        if stage.staged.file.write_all(chunk).await.is_err() {
            self.stage = None;
            self.lost = Some(SkippedKeep::Io);
        }
    }

    /// The end of the body. A key named by its content whose bytes hash to another name is an
    /// error, and nothing is kept.
    fn ended(&mut self) -> Result<(), Error> {
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
            let text = "the body does not hash to the name of the object";
            return Err(Error::Other(text.to_owned()));
        }
        Ok(())
    }
}

/// The body, passed on as it is, with each chunk given to `keeping` on the way.
fn tee<'a>(body: PartBody<'a>, keeping: &'a mut Keeping) -> PartBody<'a> {
    let state = (body, keeping, false);
    stream::unfold(state, |(mut body, keeping, done)| async move {
        if done {
            return None;
        }
        match body.next().await {
            Some(Ok(chunk)) => {
                keeping.take(&chunk).await;
                Some((Ok(chunk), (body, keeping, false)))
            }
            Some(Err(error)) => Some((Err(error), (body, keeping, true))),
            None => match keeping.ended() {
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
            Some(name) => Some(Keeping::new(
                name.content.clone(),
                self.open_stage(len).await,
            )),
            None => None,
        };
        let sent = match &mut keeping {
            Some(keeping) => self.upstream.put_stream(key, len, tee(body, keeping)).await,
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
        if let (Some(name), Some(keeping)) = (name, keeping) {
            self.keep(key, &name, keeping).await;
        }
        Ok(())
    }

    /// The copy for a body of `len` bytes: a keep permit, room for the bytes and a stage file.
    async fn open_stage(self: &Arc<Self>, len: Option<u64>) -> Result<Stage, SkippedKeep> {
        let Ok(permit) = Arc::clone(&self.keeps).try_acquire_owned() else {
            return Err(SkippedKeep::Busy);
        };
        let size = len.ok_or(SkippedKeep::Budget)?;
        let reservation = self.reserve(size).ok_or(SkippedKeep::Budget)?;
        let staged = Staged::create(self).await.map_err(|_| SkippedKeep::Io)?;
        Ok(Stage {
            staged,
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
            return;
        };
        let size = keeping.seen;
        let kept = self
            .publish_put(key, name, stage, (size, keeping.digest))
            .await;
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
        stage.staged.finish().await.map_err(|_| SkippedKeep::Io)?;
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
        let committed = self.commit(&stage.staged, name, &sidecar).await;
        committed.map_err(|_| SkippedKeep::Io)?;
        locked(&self.index).insert(&name.hex, size, meta.tag, meta.modified);
        // A delete that finished after the HEAD marked this keep (see `forget`): the object is
        // gone from the upstream.
        if self.is_invalidated(&name.hex) {
            self.drop_entry(&name.hex);
            return Err(SkippedKeep::Deleted);
        }
        drop(stage);
        self.relieve().await;
        Ok(())
    }
}
