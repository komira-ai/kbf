//! [`CacheStorage`]: the cache as a [`kbf_storage::Storage`], the boundary its callers
//! go through. A thin adapter: each call is the cache's own, unchanged; only the error
//! is converted ([`StorageError`] from [`CacheError`], answering the same gRPC code and
//! message).

use std::sync::Arc;

use bytes::Bytes;
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::ObjectStore;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_storage::{Answer, PutStream, Storage, StorageError};
use kbf_types::Digest;

use crate::cache::{Cache, CacheError, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::segments::{Connections, SegmentsConfig, SegmentsService};

/// A cache, and the `--segment-upload-*` limits its `Segments` services keep, as a
/// [`Storage`].
#[derive(Debug)]
pub struct CacheStorage<M, O> {
    cache: Arc<Cache<M, O>>,
    config: SegmentsConfig,
}

impl<M, O> CacheStorage<M, O> {
    /// `cache` as a [`Storage`], its `Segments` services keeping `config`.
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>, config: SegmentsConfig) -> Self {
        Self { cache, config }
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> Storage for CacheStorage<M, O> {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move { Ok(self.cache.find_missing(digests).await?) })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move { Ok(self.cache.read_blob(digest).await?) })
    }

    /// Hashed once here, admitted for its size as a ByteStream `Write` is
    /// ([`Cache::admit`]), and stored.
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            let size = bytes.len() as u64;
            let blob = VerifiedBlob::hashed(bytes);
            let digest = blob.digest();
            let admitted = self.cache.admit(size).await;
            self.cache.store_admitted(vec![blob], admitted).await?;
            Ok(digest)
        })
    }

    /// Always `None`: the cache cannot stream a record onto its write tier yet, so a
    /// caller stores the blob whole with [`Storage::put`].
    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    /// Its own uploads and connections: no other caller can name its uploads, and
    /// having no connection, an upload it leaves is dropped when idle.
    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(SegmentsService::new(
            Arc::clone(&self.cache),
            self.config,
            Connections::new(),
        ))
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move { Ok(self.cache.action_result(action).await?) })
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async move { Ok(self.cache.prepare_action_result(result).await?) })
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            Ok(self
                .cache
                .commit_action_record(role, action, record)
                .await?)
        })
    }
}

/// The same gRPC code and message as the [`CacheError`] answers: what the store could
/// not do (its metadata log, object store, segments, write tier, keys) becomes
/// [`StorageError::Unavailable`], carrying the cache's error as it was.
impl From<CacheError> for StorageError {
    fn from(e: CacheError) -> Self {
        match e {
            CacheError::NotFound(d) => Self::NotFound(d),
            CacheError::Unreachable(d) => Self::Unreachable(d),
            CacheError::DigestMismatch { claimed, actual } => {
                Self::DigestMismatch { claimed, actual }
            }
            CacheError::Invalid(m) => Self::Invalid(m),
            CacheError::ActionWrite(w) => Self::ActionWrite(w),
            CacheError::Internal(m) => Self::Internal(m),
            e @ (CacheError::Meta(_)
            | CacheError::Store(_)
            | CacheError::Segment(_)
            | CacheError::Tier(_)
            | CacheError::Pack(_)
            | CacheError::Key(_)) => Self::Unavailable(Arc::new(e)),
        }
    }
}
