//! [`LargeBackfill`]: the tee that copies a large blob, as a job reads it from a storage node,
//! to tierstore at `cas/l/<sha256 lowercase hex>`, so that the next reader is served from there.
//!
//! The job read listener ([`crate::job_read`]) streams a blob over 64 MiB from a storage node
//! and hashes it on the way. It hands the same chunks, at the moment it passes each on to the
//! client, to a [`Tee`], which feeds a background put of the whole blob: the blob is never held
//! whole, and the put is a streamed write of exactly the digest's size
//! ([`ObjectStore::put_stream`](kbf_objstore::ObjectStore::put_stream), a multipart upload for
//! any blob of this size, so no single request is as large as the blob).
//!
//! **Only verified bytes are stored.** The put's body ends cleanly only at an explicit marker
//! that the listener sends after the digest check; a channel that closes without it is an error
//! in the body, never an end of stream. The listener also holds back the last chunk until the
//! whole has hashed to the digest, and the tee is fed from the same points, so the put's body is
//! one chunk short of the blob until the verdict. A blob that does not hash to its digest, a
//! source that fails, and a reader that goes away all drop the [`Tee`] before its last chunk and
//! its marker: the body ends in an error, the store refuses it and leaves nothing under the key.
//!
//! **Never in the reader's way.** A chunk is handed over without waiting, into a buffer sized in
//! bytes ([`DEFAULT_BUFFER_BYTES`], two parts of the S3 store's multipart upload, so a put that
//! is busy sending one part and setting up the next still has the blob's next bytes waiting,
//! whatever the size of the chunks the reader gives); a put that falls further behind than that
//! loses its tee (the put is abandoned, counted `skipped`), and the response is not slowed. At
//! most [`DEFAULT_CONCURRENCY`] puts run at once and one blob has at most one in flight, so
//! concurrent readers of a blob cause one put; the others are `skipped`. The buffers together
//! hold at most the concurrency times the buffer size (64 MiB with the defaults).
//! Before it takes a byte the put asks the store for the key's length (one HEAD): an object of
//! the right length is `already_present` and nothing is sent; an object of another length is
//! `failed` (logged, and not overwritten: with a store that does not refuse an existing key a
//! put would replace it). A store that hangs on the HEAD or on the put holds its slot until the
//! store client's own timeout.
//!
//! **Create-only.** An existing key is refused by the store ([`ObjectStoreError::AlreadyExists`])
//! and that is success (`already_present`): the key is content-bound, so whoever wrote it wrote
//! the same bytes. A failed put is `failed`, logged, and changes nothing the reader sees.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt as _};
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_types::Digest;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};

use crate::metrics::{Counter, Families};

/// The puts running at once: a large blob is hundreds of MiB, and a few uploads are all the
/// frontend's uplink is worth. Not a flag.
pub const DEFAULT_CONCURRENCY: usize = 2;

/// What the put's body says when its tee went away before the blob verified.
const ABANDONED: &str = "the copy was abandoned before the blob hashed to its digest";

/// What a [`Tee`] hands its put: the blob's bytes in order, and then, only once the blob has
/// hashed to its digest, the marker that ends the body. A body does not end without it.
enum Piece {
    /// Bytes, with their place in the byte budget, given back when the put takes them.
    Window(Bytes, OwnedSemaphorePermit),
    Done,
}

/// The bytes a tee buffers between the reader and its put: 32 MiB, two of the 16 MiB parts the
/// S3 store sends a large object in.
pub const DEFAULT_BUFFER_BYTES: usize = 32 << 20;

/// The pieces a tee buffers whatever their size, so that a stream of tiny pieces does not make
/// a buffer of overhead.
const BUFFERED_PIECES: usize = 1 << 16;

/// What a [`LargeBackfill`] needs of the object store: two calls, over the frontend's packs
/// store in a running frontend.
pub trait LargeStore: Send + Sync {
    /// The length of the object `key`, or `None` if there is none.
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>>;

    /// Writes the object `key` of exactly `len` bytes taken from `body`, create-only. A body
    /// that ends short of `len` stores nothing.
    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>>;
}

/// The key of the large blob `digest`: `cas/l/<hex>`.
#[must_use]
pub fn large_key(digest: &Digest) -> ObjectKey {
    ObjectKey::new(format!("cas/l/{}", digest.hash_hex()))
        .expect("`cas/l/` and 64 hex digits make an object key")
}

/// How the puts ended.
#[derive(Default)]
struct Counts {
    backfilled: Counter,
    already_present: Counter,
    failed: Counter,
    skipped: Counter,
    /// Signalled each time a put ends; holds a signal if none waits ([`LargeBackfill::ended`]).
    ended: Notify,
}

impl Counts {
    fn total(&self) -> u64 {
        self.backfilled.get() + self.already_present.get() + self.failed.get() + self.skipped.get()
    }

    /// Counts an end of a put in `counter`, and tells whoever waits for ends.
    fn end(&self, counter: &Counter) {
        counter.add(1);
        self.ended.notify_one();
    }
}

/// The backfill of large blobs to tierstore. Cheap to clone.
#[derive(Clone)]
pub struct LargeBackfill {
    store: Arc<dyn LargeStore>,
    slots: Arc<Semaphore>,
    inflight: Arc<Mutex<HashSet<Digest>>>,
    counts: Arc<Counts>,
    buffer_bytes: usize,
}

impl LargeBackfill {
    /// A backfill into `store`, with at most `concurrency` (at least 1) puts at once, each
    /// buffering [`DEFAULT_BUFFER_BYTES`].
    #[must_use]
    pub fn new(store: Arc<dyn LargeStore>, concurrency: usize) -> Self {
        Self::with_buffer_bytes(store, concurrency, DEFAULT_BUFFER_BYTES)
    }

    /// [`LargeBackfill::new`], each put buffering `buffer_bytes` between the reader and the
    /// store (a test scales it down).
    #[must_use]
    pub fn with_buffer_bytes(
        store: Arc<dyn LargeStore>,
        concurrency: usize,
        buffer_bytes: usize,
    ) -> Self {
        Self {
            store,
            slots: Arc::new(Semaphore::new(concurrency.max(1))),
            inflight: Arc::default(),
            counts: Arc::default(),
            buffer_bytes,
        }
    }

    /// Waits until `n` puts in all have ended, by any result. One waiter at a time: each end
    /// wakes a single waiter, so a second concurrent caller may not be woken.
    pub async fn ended(&self, n: u64) {
        while self.counts.total() < n {
            self.counts.ended.notified().await;
        }
    }

    /// A tee for a read of `digest`, or `None` (counted `skipped`) when a put of it is already
    /// running or all the puts the bound allows are. Must run inside a tokio runtime.
    pub(crate) fn tee(&self, digest: &Digest) -> Option<Tee> {
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            self.counts.end(&self.counts.skipped);
            return None;
        };
        if !self.lock().insert(*digest) {
            self.counts.end(&self.counts.skipped);
            return None;
        }
        let (sender, chunks) = mpsc::channel::<Piece>(BUFFERED_PIECES);
        let abandoned = Arc::new(AtomicBool::new(false));
        let put = Put {
            store: Arc::clone(&self.store),
            digest: *digest,
            counts: Arc::clone(&self.counts),
            abandoned: Arc::clone(&abandoned),
            _slot: slot,
            _claim: Claim {
                inflight: Arc::clone(&self.inflight),
                digest: *digest,
            },
        };
        tokio::spawn(put.run(chunks));
        Some(Tee {
            sender,
            budget: Arc::new(Semaphore::new(self.buffer_bytes)),
            abandoned,
            finished: false,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<Digest>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends `kbf_large_backfill_total` by `result` to `out`.
    pub fn render_metrics(&self, out: &mut String) {
        let mut families = Families(std::mem::take(out));
        let name = "kbf_large_backfill_total";
        families.head(
            name,
            "counter",
            "Large blobs a job read tried to copy to tierstore, by how the copy ended",
        );
        let results = [
            ("backfilled", &self.counts.backfilled),
            ("already_present", &self.counts.already_present),
            ("failed", &self.counts.failed),
            ("skipped", &self.counts.skipped),
        ];
        for (result, counter) in results {
            let _ = writeln!(
                families.0,
                "{name}{{result=\"{result}\"}} {}",
                counter.get()
            );
        }
        *out = families.0;
    }
}

/// The hand-over of one read's verified chunks to its put. Dropped before [`Tee::finish`], it
/// abandons the put.
pub(crate) struct Tee {
    sender: mpsc::Sender<Piece>,
    /// The bytes that may wait for the put.
    budget: Arc<Semaphore>,
    abandoned: Arc<AtomicBool>,
    finished: bool,
}

impl Tee {
    /// Hands `chunk` to the put, without waiting. `false` when the put is gone or has more
    /// waiting than its byte budget: the tee is then of no use and the reader drops it.
    pub(crate) fn send(&mut self, chunk: Bytes) -> bool {
        // A chunk of more than `u32::MAX` bytes asks for more than any budget holds.
        let asked = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
        let Ok(place) = Arc::clone(&self.budget).try_acquire_many_owned(asked) else {
            return false;
        };
        self.sender.try_send(Piece::Window(chunk, place)).is_ok()
    }

    /// The blob has hashed to its digest and every chunk was sent: the put may end its body.
    ///
    /// The marker is what ends the put's body cleanly: a channel that closes without it (the
    /// tee dropped, or no room for the marker) ends the body in an error, so that the store
    /// aborts and keeps nothing.
    pub(crate) fn finish(mut self) {
        self.finished = self.sender.try_send(Piece::Done).is_ok();
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        if !self.finished {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }
}

/// The claim of one blob on the in-flight set, given up when the put ends however it ends.
struct Claim {
    inflight: Arc<Mutex<HashSet<Digest>>>,
    digest: Digest,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut inflight = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        inflight.remove(&self.digest);
    }
}

/// One blob's put, running in the background.
struct Put {
    store: Arc<dyn LargeStore>,
    digest: Digest,
    counts: Arc<Counts>,
    abandoned: Arc<AtomicBool>,
    _slot: OwnedSemaphorePermit,
    _claim: Claim,
}

impl Put {
    async fn run(self, chunks: mpsc::Receiver<Piece>) {
        let key = large_key(&self.digest);
        // An object of the right length is the blob (the key is content-bound). One of another
        // length is not, and is not written over: a store that does not refuse an existing key
        // would replace it. A store that cannot say is left to the create-only put to decide.
        if let Ok(Some(len)) = self.store.len(&key).await {
            if len == self.digest.size_bytes {
                self.counts.end(&self.counts.already_present);
            } else {
                self.counts.end(&self.counts.failed);
                tracing::warn!(
                    %key,
                    found = len,
                    wanted = self.digest.size_bytes,
                    "an object of another length is at the large key; not written over"
                );
            }
            return;
        }
        let body: BodyStream = stream::unfold(chunks, |mut chunks| async move {
            match chunks.recv().await {
                Some(Piece::Window(bytes, _place)) => Some((Ok(bytes), chunks)),
                Some(Piece::Done) => None,
                // An error ends a body; a closed channel stays closed if it is polled again.
                None => Some((Err(ObjectStoreError::Body(ABANDONED.to_owned())), chunks)),
            }
        })
        .boxed();
        match self.store.put(&key, self.digest.size_bytes, body).await {
            Ok(()) => self.counts.end(&self.counts.backfilled),
            Err(ObjectStoreError::AlreadyExists(_)) => {
                self.counts.end(&self.counts.already_present)
            }
            Err(_) if self.abandoned.load(Ordering::SeqCst) => {
                self.counts.end(&self.counts.skipped)
            }
            Err(error) => {
                self.counts.end(&self.counts.failed);
                tracing::warn!(%key, %error, "the backfill of a large blob to tierstore failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The put's body against a store that takes whatever body it is given and does not check
    //! it against the declared length, so that only the body's own end decides what is stored.

    use kbf_types::DigestFunction;

    use super::*;

    /// How the body of the last put ended: the bytes it gave if it ended cleanly, or its error.
    #[derive(Default)]
    struct Recorder {
        outcome: Mutex<Option<Result<Vec<u8>, ObjectStoreError>>>,
    }

    impl Recorder {
        fn outcome(&self) -> Result<Vec<u8>, ObjectStoreError> {
            let outcome = self.outcome.lock().unwrap_or_else(PoisonError::into_inner);
            outcome.clone().expect("a put ran")
        }
    }

    impl LargeStore for Recorder {
        fn len<'a>(
            &'a self,
            _key: &'a ObjectKey,
        ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
            Box::pin(async { Ok(None) })
        }

        fn put<'a>(
            &'a self,
            _key: &'a ObjectKey,
            _len: u64,
            mut body: BodyStream,
        ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
            Box::pin(async move {
                let mut got = Vec::new();
                let outcome = loop {
                    match body.next().await {
                        Some(Ok(piece)) => got.extend_from_slice(&piece),
                        Some(Err(error)) => break Err(error),
                        None => break Ok(got),
                    }
                };
                let answer = outcome.clone().map(|_| ());
                *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
                answer
            })
        }
    }

    fn backfill() -> (LargeBackfill, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        (LargeBackfill::new(recorder.clone(), 1), recorder)
    }

    fn digest(size: u64) -> Digest {
        Digest::new(DigestFunction::Sha256, [7; 32], size)
    }

    fn abandoned(outcome: Result<Vec<u8>, ObjectStoreError>) -> bool {
        matches!(outcome, Err(ObjectStoreError::Body(why)) if why == ABANDONED)
    }

    /// Catches: a body that takes a closed channel for the end of the blob. Every window is
    /// sent, so the body has all the bytes of the right length, and the tee is dropped without
    /// the verified marker: the body is an error, and the copy is abandoned, not stored.
    #[tokio::test]
    async fn a_tee_dropped_after_every_window_without_the_marker_is_an_error() {
        let (backfill, recorder) = backfill();
        let mut tee = backfill.tee(&digest(6)).expect("a tee");
        assert!(tee.send(Bytes::from_static(b"abc")));
        assert!(tee.send(Bytes::from_static(b"def")));
        drop(tee);
        backfill.ended(1).await;
        assert!(abandoned(recorder.outcome()));
        assert_eq!(backfill.counts.backfilled.get(), 0);
        assert_eq!(backfill.counts.skipped.get(), 1);
    }

    /// Catches: a marker lost for want of room taken as a clean end. The tee's piece queue is
    /// full when the blob has verified, so the marker cannot be sent: the body is an error.
    #[tokio::test]
    async fn a_marker_that_does_not_fit_leaves_the_body_an_error() {
        let (backfill, recorder) = backfill();
        let mut tee = backfill
            .tee(&digest(BUFFERED_PIECES as u64))
            .expect("a tee");
        for _ in 0..BUFFERED_PIECES {
            assert!(tee.send(Bytes::from_static(b"x")));
        }
        tee.finish();
        backfill.ended(1).await;
        assert!(abandoned(recorder.outcome()));
        assert_eq!(backfill.counts.backfilled.get(), 0);
    }

    /// The other side: every window and then the marker end the body cleanly, with the bytes
    /// sent in order, and the copy is backfilled.
    #[tokio::test]
    async fn every_window_and_the_marker_end_the_body_cleanly() {
        let (backfill, recorder) = backfill();
        let mut tee = backfill.tee(&digest(6)).expect("a tee");
        assert!(tee.send(Bytes::from_static(b"abc")));
        assert!(tee.send(Bytes::from_static(b"def")));
        tee.finish();
        backfill.ended(1).await;
        assert_eq!(recorder.outcome().expect("a clean end"), b"abcdef");
        assert_eq!(backfill.counts.backfilled.get(), 1);
    }

    /// Catches: a chunk over the byte budget being queued, which would hold more than the
    /// budget: it is refused and the tee is of no more use.
    #[tokio::test]
    async fn a_chunk_over_the_byte_budget_is_refused() {
        let recorder = Arc::new(Recorder::default());
        let backfill = LargeBackfill::with_buffer_bytes(recorder, 1, 4);
        let mut tee = backfill.tee(&digest(8)).expect("a tee");
        assert!(tee.send(Bytes::from_static(b"abcd")));
        assert!(!tee.send(Bytes::from_static(b"e")), "the budget is spent");
    }
}
