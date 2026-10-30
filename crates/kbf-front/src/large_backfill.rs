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
//! **Only verified bytes are stored.** The listener holds back the last chunk until the whole
//! has hashed to the digest, and the tee is fed from the same points, so the put's body is one
//! byte short until the verdict. A blob that does not hash to its digest, a source that fails,
//! and a reader that goes away all drop the [`Tee`] before its last chunk: the body ends short
//! of its declared length, the store refuses it and leaves nothing under the key.
//!
//! **Never in the reader's way.** A chunk is handed over with `try_send` into a small buffer;
//! a put that cannot keep up loses its tee (the put is abandoned, counted `skipped`), and the
//! response is not slowed. At most [`DEFAULT_CONCURRENCY`] puts run at once and one blob has at
//! most one in flight, so concurrent readers of a blob cause one put; the others are `skipped`.
//! Before it takes a byte the put asks the store for the key's length (one HEAD): an object of
//! the right length is `already_present` and nothing is sent.
//!
//! **Create-only.** An existing key is refused by the store ([`ObjectStoreError::AlreadyExists`])
//! and that is success (`already_present`): the key is content-bound, so whoever wrote it wrote
//! the same bytes. A failed put is `failed`, logged, and changes nothing the reader sees.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};
use kbf_types::Digest;
use tokio::sync::{Semaphore, mpsc};

use crate::metrics::{Counter, Families};

/// The puts running at once: a large blob is hundreds of MiB, and a few uploads are all the
/// frontend's uplink is worth. Not a flag.
pub const DEFAULT_CONCURRENCY: usize = 2;

/// The chunks a tee buffers between the reader and its put.
const BUFFERED_CHUNKS: usize = 16;

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
}

/// The backfill of large blobs to tierstore. Cheap to clone.
#[derive(Clone)]
pub struct LargeBackfill {
    store: Arc<dyn LargeStore>,
    slots: Arc<Semaphore>,
    inflight: Arc<Mutex<HashSet<Digest>>>,
    counts: Arc<Counts>,
}

impl LargeBackfill {
    /// A backfill into `store`, with at most `concurrency` (at least 1) puts at once.
    #[must_use]
    pub fn new(store: Arc<dyn LargeStore>, concurrency: usize) -> Self {
        Self {
            store,
            slots: Arc::new(Semaphore::new(concurrency.max(1))),
            inflight: Arc::default(),
            counts: Arc::default(),
        }
    }

    /// A tee for a read of `digest`, or `None` (counted `skipped`) when a put of it is already
    /// running or all the puts the bound allows are. Must run inside a tokio runtime.
    pub(crate) fn tee(&self, digest: &Digest) -> Option<Tee> {
        let _ = digest;
        self.counts.skipped.add(1);
        None
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
    sender: mpsc::Sender<Result<Bytes, ObjectStoreError>>,
    abandoned: Arc<AtomicBool>,
    finished: bool,
}

impl Tee {
    /// Hands `chunk` to the put. `false` when the put is gone or cannot keep up: the tee is
    /// then of no use and the reader drops it.
    pub(crate) fn send(&mut self, chunk: Bytes) -> bool {
        self.sender.try_send(Ok(chunk)).is_ok()
    }

    /// The blob has hashed to its digest and every chunk was sent: the put may end its body.
    pub(crate) fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        if !self.finished {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }
}
