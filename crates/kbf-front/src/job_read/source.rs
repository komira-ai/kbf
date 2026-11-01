//! [`BlobSource`]: where the blobs a job reads come from, and the frontend's [`Backend`] as one.

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;

use super::{Span, large_key};
use crate::backend::Backend;

/// Where the blobs a job reads come from.
///
/// An implementation returns the bytes that hash to `digest`, or [`StorageError::NotFound`]
/// when it holds none.
pub trait BlobSource: Send + Sync {
    /// The blob `digest`, read whole and checked against its digest.
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>>;

    /// The blob `digest` as a stream of chunks, for one too large to hold: the answer is
    /// [`StorageError::NotFound`] when the source holds none, and an error item ends a stream
    /// that fails part way. The bytes are not trusted to be the blob's: the listener hashes
    /// them as they pass.
    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>>;

    /// The blobs `digests`, one answer each in the order asked, as [`BlobSource::read_blob`]
    /// answers: for a batch, so that a source that can read blobs together (the packs a
    /// frontend keeps, where blobs that lie close are one ranged read) does. The default reads
    /// them one by one, all at once.
    fn read_many<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Vec<Result<Bytes, StorageError>>> {
        Box::pin(futures::future::join_all(
            digests.iter().map(|digest| self.read_blob(digest)),
        ))
    }

    /// Where each of `digests` lies in a pack, one answer each in the order asked, `None` for a
    /// blob with no row: for `GET /locate`, one batched ask of the pack index. The default
    /// has no packs.
    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        Box::pin(async move { Ok(vec![None; digests.len()]) })
    }

    /// The length of the object [`large_key`] names for `digest` in tierstore, `None` if there
    /// is none (a HEAD, not a read): for `GET /locate`. The default holds no large objects.
    fn large_len<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<Option<u64>, StorageError>> {
        let _ = digest;
        Box::pin(async { Ok(None) })
    }
}

impl BlobSource for Backend {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(Backend::read_blob(self, digest))
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(Backend::read_stream(self, digest))
    }

    fn read_many<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Vec<Result<Bytes, StorageError>>> {
        Box::pin(Backend::read_many(self, digests))
    }

    fn locate_packed<'a>(
        &'a self,
        digests: &'a [Digest],
    ) -> BoxFuture<'a, Result<Vec<Option<Span>>, StorageError>> {
        Box::pin(async move {
            let located = Backend::locate_packed(self, digests).await?;
            Ok(located
                .into_iter()
                .map(|at| {
                    at.map(|at| Span {
                        key: at.pack_key,
                        offset: at.offset,
                        length: at.length,
                    })
                })
                .collect())
        })
    }

    fn large_len<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<Option<u64>, StorageError>> {
        Box::pin(async move { Backend::object_len(self, &large_key(digest)).await })
    }
}
