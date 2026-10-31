//! A [`RecordStream`](super::RecordStream) with no room on a write tier (or no tier):
//! its bytes go straight to the object store as they are appended
//! ([`Cache::store_streamed_claimed`]: a multipart upload on S3, hashed as it passes),
//! and the digest is claimed at the commit. Appends pass through a queue of
//! [`STORE_CHUNKS_IN_FLIGHT`] chunks, so a stream holds a few chunks and the store's
//! part, whatever its size.
//!
//! **Not counted** in `kbf_bytes_in_flight`: each appended chunk is copied
//! (`Bytes::copy_from_slice`) into that queue, so up to [`STORE_CHUNKS_IN_FLIGHT`] copies
//! and the store's part are resident beside the counted message they were copied from;
//! the gauge understates resident memory by that much.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::FutureExt as _;
use futures::stream::StreamExt as _;
use kbf_objstore::{ObjectStore, ObjectStoreError};
use kbf_segments::Crc32c;
use kbf_types::Digest;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::stream::checked;
use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

/// The most chunks appended to a stream to the store and not yet taken by it: the
/// appender waits for room past it.
pub(crate) const STORE_CHUNKS_IN_FLIGHT: usize = 2;

/// Why an append failed when the upload task ended without an error of its own to give.
/// A task that ends normally before the claim cannot (the claim is held until the
/// commit, so only a claim withdrawn ends it with no result), so this is what is left
/// when the task panicked or its runtime shut down.
const STOPPED: &str = "the store stopped taking the streamed blob";

/// The upload of one streamed record to the store, running from its first append.
type Upload = JoinHandle<Result<Option<Digest>, CacheError>>;

/// A stream to the store, being appended to.
pub(super) struct ToStore {
    bytes: mpsc::Sender<Result<Bytes, ObjectStoreError>>,
    claim: oneshot::Sender<Digest>,
    upload: Upload,
    /// The runtime `upload` runs on, to wait for its end from the appending thread.
    runtime: tokio::runtime::Handle,
    /// Why the upload ended, once an append found it gone: the same answer for every
    /// append after.
    refused: Option<CacheError>,
    size: u64,
    written: u64,
    crc: Crc32c,
}

/// A stream to the store whose bytes were all appended and checked; the store completes
/// its upload once the digest is claimed ([`Claim::commit`]). Dropped, the claim is
/// withdrawn: the upload is aborted and nothing is stored.
pub(super) struct Claim {
    claim: oneshot::Sender<Digest>,
    upload: Upload,
    /// The upload's end, if an append took it: its task was consumed, so the commit
    /// answers this instead of waiting on it again.
    refused: Option<CacheError>,
}

impl ToStore {
    /// Starts the upload of a record of `size` bytes on the cache's runtime; `None` for
    /// a size of 0, or a cache made outside a runtime (it cannot stream at all).
    pub(super) fn start<M: MetaLog, O: ObjectStore + 'static>(
        cache: &Arc<Cache<M, O>>,
        size: u64,
    ) -> Option<Self> {
        let runtime = cache.runtime.clone()?;
        (size > 0).then_some(())?;
        let (bytes, queued) = mpsc::channel(STORE_CHUNKS_IN_FLIGHT);
        let (claim, claimed) = oneshot::channel();
        let body = futures::stream::unfold(queued, |mut queued| async move {
            let chunk = queued.recv().await?;
            Some((chunk, queued))
        })
        .boxed();
        let claimed = async move { claimed.await.ok() }.boxed();
        let cache = Arc::clone(cache);
        let upload =
            runtime.spawn(async move { cache.store_streamed_claimed(size, body, claimed).await });
        Some(Self {
            bytes,
            claim,
            upload,
            runtime,
            refused: None,
            size,
            written: 0,
            crc: Crc32c::new(),
        })
    }

    /// Queues a copy of `chunk` for the store, waiting for room. Blocking. An upload that
    /// has ended fails the append with the error it ended with (the store's own).
    pub(super) fn append(&mut self, chunk: &[u8]) -> Result<(), CacheError> {
        let after = self.written + chunk.len() as u64;
        if after > self.size {
            return Err(CacheError::Invalid(format!(
                "{after} bytes appended to a stream made for {}",
                self.size
            )));
        }
        // A queue the upload no longer reads has no room for ever: the upload ended, and
        // its own error is the answer.
        if self.refused.is_some()
            || self
                .bytes
                .blocking_send(Ok(Bytes::copy_from_slice(chunk)))
                .is_err()
        {
            return Err(self.ended());
        }
        self.crc.update(chunk);
        self.written = after;
        Ok(())
    }

    /// The error the upload ended with, taken from its task (which has ended or is
    /// ending: its body is dropped, and it only settles the store's write). Blocking: on
    /// S3 the appender waits out the abort of the multipart upload, which the store's
    /// `request_timeout` bounds.
    fn ended(&mut self) -> CacheError {
        // Built whether or not it is needed: no branch of its own.
        let gone = CacheError::Invalid(STOPPED.to_owned());
        let (runtime, upload) = (&self.runtime, &mut self.upload);
        self.refused
            .get_or_insert_with(|| {
                runtime
                    .block_on(upload)
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or(gone)
            })
            .clone()
    }

    /// Ends the bytes as the blob `digest` with CRC-32C `crc32c`, checked as a tier
    /// stream's are; the upload then waits for the claim.
    pub(super) fn finish(self, digest: Digest, crc32c: u32) -> Result<Claim, CacheError> {
        checked(self.written, self.size, digest, self.crc.finish(), crc32c)?;
        let Self {
            claim,
            upload,
            refused,
            ..
        } = self;
        Ok(Claim {
            claim,
            upload,
            refused,
        })
    }
}

impl Claim {
    /// Claims `digest`: the store completes the upload only if the bytes hash to it,
    /// and the blob is indexed. Answers once it is present.
    pub(super) async fn commit(self, digest: Digest) -> Result<(), CacheError> {
        if let Some(refused) = self.refused {
            return Err(refused);
        }
        let _ = self.claim.send(digest);
        // An upload that never ended (its runtime shut down) stored nothing.
        let gone = Err(CacheError::Invalid(STOPPED.to_owned()));
        self.upload.await.unwrap_or(gone).map(drop)
    }
}
