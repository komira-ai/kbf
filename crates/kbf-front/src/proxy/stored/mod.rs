//! A frontend with no storage node: [`Stored`], the client-facing services and the action cache
//! over the packs, the pack index and the action cache table of the state store, and nothing
//! else (`docs/design/jobs-on-jobscheduler-and-tierstore.md`).
//!
//! Where [`Proxy`](super::Proxy) hands a client's calls to storage nodes, and asks the packs
//! first, this holds everything itself:
//!
//! - **Which blobs are held** is the pack index, whatever their size: a blob in a pack, and a
//!   large blob stored as the object `cas/l/<sha256>` and recorded as a pack of one blob
//!   ([`Packs::put_large`]). FindMissingBlobs asks the index and nothing else.
//! - **Reads** are ranged reads of the object the index names: a blob within
//!   [`Packs::max_blob_bytes`] whole (BatchReadBlobs, ByteStream Read), any blob in windows
//!   (ByteStream Read, [`Packs::read_large`]).
//! - **Uploads** are written by the frontend: a blob within the bound is received whole,
//!   hashed and packed; a larger one is streamed to `cas/l/<sha256>` as it arrives (hashed on
//!   the way, never held whole) and recorded. A compressed upload or read (`compressed-blobs/`)
//!   is INVALID_ARGUMENT until it is needed. `QueryWriteStatus` says a blob the index has is
//!   complete, and for one whose upload is in flight in this process, how many bytes of it
//!   have arrived (the upload still starts at 0: nothing resumes).
//! - **The action cache** is the `action_result` table of the state store
//!   ([`ActionStore`](kbf_pack_index::ActionStore)): a lookup decodes the row's `ActionResult`
//!   and is a hit only if every blob it names is in the index ([`Stored::action_result`]); a
//!   write checks the same and stores the encoded result in one row ([`Stored::put_action_result`]).
//!   A client never writes the action cache.
//!
//! No call dials anything. What is not here: GetTree, SplitBlob, SpliceBlob and the chunk
//! mapping calls (UNIMPLEMENTED, as in the proxy), compressed blobs, and any probe of the
//! object store: a store that is down is found by the call that needs it, which fails
//! UNAVAILABLE.

mod ac;
mod bytestream;
mod cas;
mod uploads;

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_auth::Authorizers;
use kbf_objstore::ObjectStore;
use kbf_pack::{PackError, PackKeys};
use kbf_pack_index::action::ActionStore;
use kbf_pack_index::{BlobDigest, PackIndex, PackIndexError};
use kbf_proto::reapi::ActionResult;
use kbf_storage::StorageError;
use kbf_types::Digest;
use prost::Message as _;
use tonic::Status;
use tonic::service::Routes;

use self::uploads::Uploads;
use super::Packs;
use super::packed::PackSettings;
use crate::MAX_MESSAGE_BYTES;
use crate::backend::{Layered, checked, served};
use crate::input_walk::Blobs;

type Answer<T> = Result<T, StorageError>;

/// The blobs, the large objects and the action cache of a frontend with no storage node.
/// Cheap to share: hold it in an `Arc`.
pub struct Stored {
    packs: Packs,
    actions: Arc<dyn ActionStore>,
    uploads: Arc<Uploads>,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored").finish_non_exhaustive()
    }
}

impl Stored {
    /// Packs written to `store` under `keys`, recorded in `actions` (which is the pack index
    /// and the action cache, so that the two are one handle and a result and the blobs it names
    /// are found through the same rows), as `settings` say.
    ///
    /// # Errors
    /// [`PackError::NotCreateOnly`] when the store does not claim create-only writes.
    pub fn new<S: ObjectStore>(
        store: Arc<S>,
        actions: Arc<dyn ActionStore>,
        keys: PackKeys,
        settings: PackSettings,
    ) -> Result<Self, PackError> {
        let index: Arc<dyn PackIndex> = actions.clone();
        let packs = Packs::new(store, index, keys, settings)?;
        Ok(Self {
            packs,
            actions,
            uploads: Arc::default(),
        })
    }

    /// The packs: the blobs, the large objects and the reads of them.
    #[must_use]
    pub fn packs(&self) -> &Packs {
        &self.packs
    }

    /// The action-cache entry for `action`: the digest of its encoded result and the result,
    /// if the action has a row and every blob the result names is held; a row some blob of which
    /// is gone is a miss.
    ///
    /// # Errors
    /// The state store could not answer, or its row does not decode as an `ActionResult`.
    pub async fn action_result(&self, action: &Digest) -> Answer<Option<(Digest, ActionResult)>> {
        let asked = to_index(action);
        let actions = Arc::clone(&self.actions);
        let row = blocking(move || actions.get_action(&asked)).await?;
        let Some(entry) = row.map_err(index_error)? else {
            return Ok(None);
        };
        let result = ActionResult::decode(entry.result.as_slice()).map_err(|e| {
            StorageError::Internal(format!(
                "the action cache row of {action} does not decode: {e}"
            ))
        })?;
        let hit = Some((kbf_segments::sha256(&entry.result), result));
        served(hit, &self.held()).await
    }

    /// Writes `result` as the action cache entry of `action`, replacing an entry it has; the
    /// result is checked first, and refused unless every blob it names is held.
    ///
    /// # Errors
    /// [`StorageError::ActionWrite`] naming the blobs that are not held; a result no row holds
    /// ([`StorageError::Invalid`]); the state store could not answer.
    pub async fn put_action_result(&self, action: &Digest, result: &ActionResult) -> Answer<()> {
        checked(result, &self.held()).await?;
        let encoded = result.encode_to_vec();
        let (actions, asked) = (Arc::clone(&self.actions), to_index(action));
        let put = blocking(move || actions.put_action(&asked, &encoded, &[])).await?;
        put.map_err(index_error)?;
        Ok(())
    }

    /// The blobs as the closure check reads them: the packs, and no other node.
    pub(crate) fn held(&self) -> Held<'_> {
        Held(&self.packs)
    }

    /// `ContentAddressableStorage`, `ByteStream` and `ActionCache` over this, each call
    /// authorized by `authorizers`.
    #[must_use]
    pub fn routes(self: &Arc<Self>, authorizers: &Arc<Authorizers>) -> Routes {
        let cas = cas::StoredCas {
            stored: Arc::clone(self),
            authorizers: Arc::clone(authorizers),
        };
        let bytestream = bytestream::StoredByteStream {
            stored: Arc::clone(self),
            authorizers: Arc::clone(authorizers),
        };
        let action_cache = ac::StoredActionCache {
            stored: Arc::clone(self),
            authorizers: Arc::clone(authorizers),
        };
        Routes::new(
            kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorageServer::new(cas)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES),
        )
        .add_service(kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer::new(
            bytestream,
        ))
        .add_service(
            kbf_proto::reapi::action_cache_server::ActionCacheServer::new(action_cache),
        )
    }
}

/// The packs as the closure check reads them, with no node behind them: a blob the index lacks
/// is missing, and no other place is asked.
pub(crate) struct Held<'a>(&'a Packs);

impl Blobs for Held<'_> {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(async move {
            let absent = Absent;
            Layered {
                packs: self.0,
                rest: &absent,
            }
            .read_blob(digest)
            .await
        })
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move {
            let absent = Absent;
            Layered {
                packs: self.0,
                rest: &absent,
            }
            .find_missing(digests)
            .await
        })
    }
}

/// Where no blob is: what is left under the packs when there is no node.
pub(crate) struct Absent;

impl Blobs for Absent {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(futures::future::ready(Err(StorageError::NotFound(*digest))))
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(futures::future::ready(Ok(digests.to_vec())))
    }
}

/// The index's form of a digest.
pub(crate) fn to_index(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// A call of the state store, which blocks, run on the runtime's blocking pool.
///
/// # Errors
/// The call panicked or the runtime is shutting down.
pub(crate) async fn blocking<T, F>(call: F) -> Answer<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|e| StorageError::Internal(format!("a state store call did not finish: {e}")))
}

/// A refusal of the state store as the store's: a result it cannot hold is invalid, and
/// anything else is the store being unavailable.
pub(crate) fn index_error(error: PackIndexError) -> StorageError {
    match error {
        PackIndexError::Invalid(why) => StorageError::Invalid(format!("the action cache: {why}")),
        other => StorageError::Unavailable(Arc::new(Status::unavailable(format!(
            "state store: {other}"
        )))),
    }
}
