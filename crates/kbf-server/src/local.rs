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
//! Every call goes through the storage boundary, an `Arc<dyn kbf_storage::Storage>`
//! ([`LocalStore::over`]); [`LocalStore::new`] puts the cache behind it with
//! `kbf-front`'s adapter ([`CacheStorage`]), which calls the cache's own code unchanged.
//!
//! Not here yet: admission. The worker listener admits every remote call by the
//! daemon's certificate and the deny list; a local call is admitted by nothing.

use std::sync::Arc;

use kbf_front::{Cache, CacheStorage, MetaLog, SegmentsConfig};
use kbf_objstore::ObjectStore;
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;

/// The server's cache, as calls a daemon in the same process makes, through the
/// storage boundary ([`kbf_storage::Storage`]). Clones share the store.
#[derive(Clone)]
pub struct LocalStore {
    inner: Arc<dyn Storage>,
}

impl LocalStore {
    /// The calls over `cache` ([`CacheStorage`]), its `Segments` services keeping
    /// `config`.
    pub fn new<M: MetaLog, O: ObjectStore + 'static>(
        cache: Arc<Cache<M, O>>,
        config: SegmentsConfig,
    ) -> Self {
        Self::over(Arc::new(CacheStorage::new(cache, config)))
    }

    /// The calls over any `storage`.
    #[must_use]
    pub fn over(storage: Arc<dyn Storage>) -> Self {
        Self { inner: storage }
    }

    /// The digests among `digests` the cache does not hold, in the order asked, as
    /// FindMissingBlobs answers; the held ones are touched.
    ///
    /// # Errors
    /// The metadata log is unavailable.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        self.inner.find_missing(digests).await
    }

    /// The blob's bytes, checked against its digest by the cache.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if the blob is not held, or why it cannot be read.
    pub async fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        self.inner.read_blob(digest).await.map(Vec::from)
    }

    /// Hashes `bytes` once, stores them under that digest within
    /// `--write-bytes-in-flight` (waiting for room, as a ByteStream `Write` does), and
    /// returns the digest once they are durable.
    ///
    /// # Errors
    /// The blob is over the largest the cache accepts, or the store or the metadata
    /// log failed.
    pub async fn put(&self, bytes: Vec<u8>) -> Result<Digest, StorageError> {
        self.inner.put(bytes.into()).await
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
