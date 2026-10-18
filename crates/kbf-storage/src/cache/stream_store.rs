//! A [`RecordStream`](super::RecordStream) with no room on a write tier (or no tier):
//! its bytes go straight to the object store as they are appended
//! ([`Cache::store_streamed_claimed`]: a multipart upload on S3, hashed as it passes),
//! and the digest is claimed at the commit. Appends pass through a queue of
//! [`STORE_CHUNKS_IN_FLIGHT`] chunks, so a stream holds a few chunks and the store's
//! part, whatever its size.

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

/// Why an append failed once the store's write stopped (it failed, and its commit
/// answers why).
const STOPPED: &str = "the store stopped taking the streamed blob";

/// The upload of one streamed record to the store, running from its first append.
type Upload = JoinHandle<Result<Option<Digest>, CacheError>>;

/// A stream to the store, being appended to.
pub(super) struct ToStore {
    bytes: mpsc::Sender<Result<Bytes, ObjectStoreError>>,
    claim: oneshot::Sender<Digest>,
    upload: Upload,
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
}

impl ToStore {
    /// Starts the upload of a record of `size` bytes on the cache's runtime; `None` for
    /// a size of 0, or a cache made outside a runtime (it cannot stream at all).
    pub(super) fn start<M: MetaLog, O: ObjectStore + 'static>(
        cache: &Arc<Cache<M, O>>,
        size: u64,
    ) -> Option<Self> {
        let runtime = cache.runtime.as_ref()?;
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
            size,
            written: 0,
            crc: Crc32c::new(),
        })
    }

    /// Queues a copy of `chunk` for the store, waiting for room. Blocking.
    pub(super) fn append(&mut self, chunk: &[u8]) -> Result<(), CacheError> {
        let after = self.written + chunk.len() as u64;
        if after > self.size {
            return Err(CacheError::Invalid(format!(
                "{after} bytes appended to a stream made for {}",
                self.size
            )));
        }
        let queued = self.bytes.blocking_send(Ok(Bytes::copy_from_slice(chunk)));
        // Built whether or not it is needed: no branch of its own.
        queued.or(Err(CacheError::Invalid(STOPPED.to_owned())))?;
        self.crc.update(chunk);
        self.written = after;
        Ok(())
    }

    /// Ends the bytes as the blob `digest` with CRC-32C `crc32c`, checked as a tier
    /// stream's are; the upload then waits for the claim.
    pub(super) fn finish(self, digest: Digest, crc32c: u32) -> Result<Claim, CacheError> {
        checked(self.written, self.size, digest, self.crc.finish(), crc32c)?;
        let Self { claim, upload, .. } = self;
        Ok(Claim { claim, upload })
    }
}

impl Claim {
    /// Claims `digest`: the store completes the upload only if the bytes hash to it,
    /// and the blob is indexed. Answers once it is present.
    pub(super) async fn commit(self, digest: Digest) -> Result<(), CacheError> {
        let _ = self.claim.send(digest);
        // An upload that never ended (its runtime shut down) stored nothing.
        let gone = Err(CacheError::Invalid(STOPPED.to_owned()));
        self.upload.await.unwrap_or(gone).map(drop)
    }
}
