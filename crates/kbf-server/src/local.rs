//! The server's blob store as calls in this process: [`LocalStore`], what a worker
//! daemon linked into the server's own process (`kbf-all-in-one`) reads and writes blobs
//! through, with no listener, TLS or protobuf encoding between them.
//!
//! Each call is one the worker listener serves a remote daemon, on the same [`Cache`]:
//! FindMissing ([`LocalStore::find_missing`]), a whole-blob read
//! ([`LocalStore::read_blob`]), a whole-blob write hashed here once, under the same
//! admission as a ByteStream `Write` ([`LocalStore::put`]), and the `Segments` service
//! itself ([`LocalStore::segments`]), whose `Begin`, `PutRange` and `Commit` are the
//! handlers a remote upload runs: with a write tier the daemon's digests are trusted at
//! the commit and the flusher hashes every record before its PUT, dropping one that
//! does not match; without one each record is hashed as its ranges arrive. Nothing here
//! skips a check the listener's path makes.
//!
//! Not here yet: admission. The worker listener admits every remote call by the
//! daemon's certificate and the deny list; a local call is admitted by nothing.

use std::sync::Arc;

use futures::future::BoxFuture;
use kbf_front::{
    Cache, CacheError, Connections, MetaLog, SegmentsConfig, SegmentsService, VerifiedBlob,
};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::segments_server::Segments;
use kbf_types::Digest;

/// The server's cache, as calls a daemon in the same process makes. Clones share the
/// cache.
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<dyn Blobs>,
}

/// [`LocalStore`]'s calls over a cache of any metadata log and object store.
trait Blobs: Send + Sync {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>>;
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Vec<u8>>;
    fn put(&self, bytes: Vec<u8>) -> Answer<'_, Digest>;
    fn segments(&self) -> Arc<dyn Segments>;
}

type Answer<'a, T> = BoxFuture<'a, Result<T, CacheError>>;

/// A cache and the `--segment-upload-*` limits its `Segments` services keep.
struct Over<M, O> {
    cache: Arc<Cache<M, O>>,
    config: SegmentsConfig,
}

impl<M: MetaLog, O: ObjectStore + 'static> Blobs for Over<M, O> {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(self.cache.find_missing(digests))
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Vec<u8>> {
        Box::pin(async move { self.cache.read_blob(digest).await.map(Vec::from) })
    }

    fn put(&self, bytes: Vec<u8>) -> Answer<'_, Digest> {
        Box::pin(async move {
            let size = bytes.len() as u64;
            let blob = VerifiedBlob::hashed(bytes.into());
            let digest = blob.digest();
            let admitted = self.cache.admit(size).await;
            let stored = self.cache.store_admitted(vec![blob], admitted).await;
            stored.map(|()| digest)
        })
    }

    fn segments(&self) -> Arc<dyn Segments> {
        // Its own uploads and connections: no other caller can name its uploads, and
        // having no connection, an upload it leaves is dropped when idle.
        let connections = Connections::new();
        Arc::new(SegmentsService::new(
            Arc::clone(&self.cache),
            self.config,
            connections,
        ))
    }
}

impl LocalStore {
    /// The calls over `cache`, its `Segments` services keeping `config`.
    pub fn new<M: MetaLog, O: ObjectStore + 'static>(
        cache: Arc<Cache<M, O>>,
        config: SegmentsConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Over { cache, config }),
        }
    }

    /// The digests among `digests` the cache does not hold, in the order asked, as
    /// FindMissingBlobs answers; the held ones are touched.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CacheError> {
        self.inner.find_missing(digests).await
    }

    /// The blob's bytes, checked against its digest by the cache.
    ///
    /// # Errors
    /// [`CacheError::NotFound`] if the blob is not held, or why it cannot be read.
    pub async fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>, CacheError> {
        self.inner.read_blob(digest).await
    }

    /// Hashes `bytes` once, stores them under that digest within
    /// `--write-bytes-in-flight` (waiting for room, as a ByteStream `Write` does), and
    /// returns the digest once they are durable.
    ///
    /// # Errors
    /// The blob is over the largest the cache accepts, or the store or the metadata
    /// log failed.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CacheError> {
        self.inner.put(bytes).await
    }

    /// A `Segments` service of its own over the cache: the one a remote upload's calls
    /// are served by, with uploads no other caller can name. One per caller; an upload
    /// is held until it commits, or until it has had no call for
    /// `--segment-upload-idle-secs`.
    #[must_use]
    pub fn segments(&self) -> Arc<dyn Segments> {
        self.inner.segments()
    }
}
