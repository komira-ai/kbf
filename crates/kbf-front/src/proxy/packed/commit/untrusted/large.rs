//! A large object of a run: [`Large`] streams the raw object a task wrote under an `L<i>` key
//! from the object store, through SHA-256, into the CAS, a window at a time.
//!
//! The object is never held whole. The object store is read in windows of [`WINDOW_BYTES`], each
//! read only when the CAS write asks for the next piece, so this process holds one window (and the
//! write's own piece in flight) however large the object is. The bytes are hashed as they pass and
//! the write names the digest the manifest gave, so a store of the blob keeps nothing under that
//! digest unless the bytes are its own: this module says which of the two refused (the bytes, or the
//! store) from what it hashed, whatever error the store answered.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStoreError};
use kbf_segments::RecordHasher;
use kbf_storage::PieceStream;
use kbf_types::Digest;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::task::{RejectReason, Step, Stop, unavailable};
use super::{IssuedPackKey, Packs};
use crate::backend::Backend;
use crate::proxy::packed::LargeWriteThrough;

/// The most bytes of a large object read from the object store at once.
pub(super) const WINDOW_BYTES: u64 = 8 << 20;

/// How long the write-through may leave a window unaccepted before it is given up on: the CAS
/// write is not held up longer than this by the object store the copy goes to.
const COPY_STALL: Duration = Duration::from_secs(30);

/// How long the write-through may take to finish once the CAS write has ended (the length
/// question, the last part, the completion, the store client's own retries) before it is
/// abandoned and counted failed: the ingest does not wait on the copy longer than this.
const COPY_FINISH: Duration = Duration::from_secs(120);

/// What the CAS side tells the write-through. A body that ends is complete only after [`Done`],
/// which is sent once the bytes have hashed to the digest: a channel that closes without it (the
/// CAS side stopped polling, the ingest was cancelled, the bytes were not the blob) fails the write.
///
/// [`Done`]: CopyPiece::Done
enum CopyPiece {
    /// The next window of the object.
    Window(Bytes),
    /// Every byte was sent and they hash to the digest.
    Done,
}

/// How a pass over the object ended.
enum Ended {
    /// Every byte was read and they hash to this.
    Read(Digest),
    /// The read stopped, for this reason.
    Stopped(Stop),
}

/// What a pass over the object has seen.
#[derive(Default)]
struct Pass {
    hasher: RecordHasher,
    ended: Option<Ended>,
    /// The write-through of this pass to the object store.
    wrote: Option<JoinHandle<()>>,
}

fn locked(pass: &Mutex<Pass>) -> MutexGuard<'_, Pass> {
    pass.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One large object of a run: the object under its key, and the blob the manifest says it is.
pub(super) struct Large<'a> {
    packs: &'a Packs,
    object: ObjectKey,
    digest: Digest,
    pass: Arc<Mutex<Pass>>,
}

impl<'a> Large<'a> {
    /// The object under `key`, which the manifest says is the blob `digest`.
    pub(super) fn new(packs: &'a Packs, key: &IssuedPackKey, digest: Digest) -> Self {
        Self {
            packs,
            object: key.object.clone(),
            digest,
            pass: Arc::default(),
        }
    }

    /// Checks the object's length: it must be there and as long as the digest says. An object of
    /// another length is not that blob (`HashMismatch`), found before a byte is read.
    pub(super) async fn check_length(&self) -> Step<()> {
        match self.packs.store.len(&self.object).await {
            Ok(Some(len)) if len == self.digest.size_bytes => Ok(()),
            Ok(Some(_)) => Err(RejectReason::HashMismatch.into()),
            Ok(None) => Err(RejectReason::Incomplete.into()),
            Err(error) => Err(unavailable(&format!(
                "the length of {}: {error}",
                self.object
            ))),
        }
    }

    /// Streams the object into the CAS through `backend`.
    ///
    /// The object store is read again from the first byte if the write has to be made again (the
    /// next owner of the blob, a newer map).
    ///
    /// # Errors
    /// `HashMismatch` if the bytes (all of them, or the last window cut short) are not the blob,
    /// and nothing is kept under its digest; `Incomplete` if the object is gone; `Unavailable` if
    /// the object store or the CAS failed.
    pub(super) async fn store(&self, backend: &Backend) -> Step<()> {
        let open = || self.open();
        let stored = backend.put_streamed(&self.digest, &open).await;
        let wrote = locked(&self.pass).wrote.take();
        if let Some(mut wrote) = wrote {
            // The copy ends with the stream it was fed from; its result is counted by its task. A
            // copy that does not end in time is abandoned (a dropped upload is aborted).
            if tokio::time::timeout(COPY_FINISH, &mut wrote).await.is_err() {
                wrote.abort();
                tracing::warn!(object = %self.object, "the write-through of a large object did not finish in time");
                let failed = &self.packs.metrics.ingest_large_writethrough;
                failed[LargeWriteThrough::Failed as usize].add(1);
            }
        }
        let ended = locked(&self.pass).ended.take();
        match (ended, stored) {
            (Some(Ended::Read(actual)), stored) if actual == self.digest => {
                stored.map_err(|error| unavailable(&format!("storing {}: {error}", self.digest)))
            }
            (Some(Ended::Read(_)), _) => Err(RejectReason::HashMismatch.into()),
            (Some(Ended::Stopped(stop)), _) => Err(stop),
            (None, stored) => Err(unavailable(&format!(
                "the CAS took no bytes of {}: {stored:?}",
                self.object
            ))),
        }
    }

    /// The pieces of the object from its first byte. Starts the pass over.
    fn open(&self) -> PieceStream {
        let (tee, wrote) = self.copy().unzip();
        *locked(&self.pass) = Pass {
            wrote,
            ..Pass::default()
        };
        let reading = Reading {
            packs: self.packs.clone(),
            object: self.object.clone(),
            digest: self.digest,
            at: 0,
            pass: Arc::clone(&self.pass),
            tee,
        };
        stream::try_unfold(reading, Reading::step).boxed()
    }

    /// Starts the write of the object to `cas/l/<sha256 hex>`: the sender of the windows it is to
    /// write and the task that writes them. The task counts how the write ended.
    fn copy(&self) -> Option<(mpsc::Sender<CopyPiece>, JoinHandle<()>)> {
        let hex: String = self
            .digest
            .hash
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let key = ObjectKey::new(format!("cas/l/{hex}")).ok()?;
        let (send, windows) = mpsc::channel::<CopyPiece>(1);
        let body = stream::unfold(windows, |mut windows| async move {
            match windows.recv().await {
                Some(CopyPiece::Window(bytes)) => Some((Ok(bytes), windows)),
                Some(CopyPiece::Done) => None,
                None => {
                    let given_up = ObjectStoreError::Body("the write was given up".to_owned());
                    Some((Err(given_up), windows))
                }
            }
        })
        .boxed();
        let store = Arc::clone(&self.packs.store);
        let metrics = Arc::clone(&self.packs.metrics);
        let size = self.digest.size_bytes;
        let task = tokio::spawn(async move {
            let how = match store.len(&key).await {
                // Held whole already: the key names the bytes, so there is nothing to upload.
                Ok(Some(held)) if held == size => LargeWriteThrough::Exists,
                // An object of another length under a key that names these bytes is not trusted
                // and is not replaced (a create-only write would be refused): a failure.
                Ok(Some(held)) => {
                    tracing::warn!(%key, held, size, "a large object's key holds another length");
                    LargeWriteThrough::Failed
                }
                // Not there, or the store could not say: write, and let the create-only rule decide.
                Ok(None) | Err(_) => match store.put_stream(&key, size, body).await {
                    Ok(()) => LargeWriteThrough::Ok,
                    Err(ObjectStoreError::AlreadyExists(_)) => LargeWriteThrough::Exists,
                    // The body ended short or gave an error: the ingest gave the write up.
                    Err(ObjectStoreError::Body(why)) => {
                        tracing::debug!(%key, why, "the write-through of a large object was given up");
                        LargeWriteThrough::Aborted
                    }
                    Err(error) => {
                        tracing::warn!(%key, %error, "the write-through of a large object failed");
                        LargeWriteThrough::Failed
                    }
                },
            };
            metrics.ingest_large_writethrough[how as usize].add(1);
        });
        Some((send, task))
    }
}

/// The state of the stream of one pass: where the next window starts.
struct Reading {
    packs: Packs,
    object: ObjectKey,
    digest: Digest,
    at: u64,
    pass: Arc<Mutex<Pass>>,
    /// The windows of the write-through, until it is given up on or ends.
    tee: Option<mpsc::Sender<CopyPiece>>,
}

impl Reading {
    /// The next window, or the end of the object (which says what its bytes hash to).
    async fn step(mut self) -> io::Result<Option<(Bytes, Self)>> {
        let size = self.digest.size_bytes;
        if self.at == size {
            let seen = {
                let mut pass = locked(&self.pass);
                let (seen, _) = std::mem::take(&mut pass.hasher).finish();
                pass.ended = Some(Ended::Read(seen));
                seen
            };
            // The end is said only for the blob's own bytes; anything else leaves the channel to
            // close, which fails the write.
            if let Some(tee) = self.tee.take().filter(|_| seen == self.digest) {
                let _ = tee.send(CopyPiece::Done).await;
            }
            return Ok(None);
        }
        let want = WINDOW_BYTES.min(size - self.at);
        let range = ByteRange::new(self.at, want).expect("a window has bytes");
        match self.packs.fetch(&self.object, range).await {
            Ok(bytes) if bytes.len() as u64 == want => {
                locked(&self.pass).hasher.update(&bytes);
                self.at += want;
                self.copy(&bytes).await;
                Ok(Some((bytes, self)))
            }
            Ok(_) => Err(self.stopped(
                RejectReason::HashMismatch.into(),
                "the object is shorter than its length said",
            )),
            Err(error) => {
                let stop = Stop::from(error).absent_is_incomplete();
                Err(self.stopped(stop, "reading the object failed"))
            }
        }
    }

    /// Hands `window` to the write-through, which is dropped (and its write ends short, so the
    /// store keeps nothing) if it has ended or does not take the window in time.
    async fn copy(&mut self, window: &Bytes) {
        let Some(tee) = &self.tee else { return };
        let sent =
            tokio::time::timeout(COPY_STALL, tee.send(CopyPiece::Window(window.clone()))).await;
        if !matches!(sent, Ok(Ok(()))) {
            self.tee = None;
        }
    }

    /// Notes that the pass stopped for `stop`, and the error that ends the stream.
    fn stopped(&self, stop: Stop, why: &str) -> io::Error {
        locked(&self.pass).ended = Some(Ended::Stopped(stop));
        io::Error::other(format!("{}: {why}", self.object))
    }
}
