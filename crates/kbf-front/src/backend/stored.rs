//! The [`Backend`](super::Backend) of a frontend with no storage node: [`Stored`], and nothing
//! behind it. Every call is the packs' and the state store's: what is held is the pack index,
//! the action cache is its table, and what Execute and the scheduler write is written to the
//! packs' object store.
//!
//! The action-cache write has one half here. [`Stored::put_action_result`] checks the closure
//! and stores the encoded result inline in the row of the action (there is no result blob to
//! store first), so a prepared [`ActionRecord`] has nothing to commit: [`Plane::prepare_action_result`]
//! answers the check, and [`Plane::commit_action_record`] is refused. A caller that writes a
//! result uses [`Plane::put_action_result`].

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_meta::ActionRecord;
use kbf_proto::reapi::ActionResult;
use kbf_storage::{OpenPieces, ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use tonic::Code;

use super::layered::unavailable;
use super::{Answer, Layered, Plane, checked};
use crate::input_tree::Walked;
use crate::input_walk::Blobs as _;
use crate::input_walk::{Trees, walk};
use crate::meta_log::Leadership;
use crate::metrics::CacheMetrics;
use crate::proxy::Absent;
use crate::proxy::{Packs, Stored};

/// A [`Stored`], and the metrics of the service that calls it.
pub(super) struct StoredPlane {
    stored: Arc<Stored>,
    metrics: CacheMetrics,
    trees: Trees,
}

impl StoredPlane {
    pub(super) fn new(stored: Arc<Stored>) -> Self {
        Self {
            stored,
            metrics: CacheMetrics::default(),
            trees: Trees::default(),
        }
    }

    fn packs(&self) -> &Packs {
        self.stored.packs()
    }
}

impl Plane for StoredPlane {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move { self.packs().missing(digests).await.map_err(unavailable) })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(async move {
            let packs = self.packs();
            if digest.size_bytes > packs.max_blob_bytes() {
                // A blob too large to read whole is held, or it is not.
                let held = packs.locate_stored(digest).await.map_err(unavailable)?;
                return Err(match held {
                    Some(_) => StorageError::Invalid(format!(
                        "blob {digest} is over the {} bytes that are read whole: read it as a stream",
                        packs.max_blob_bytes()
                    )),
                    None => StorageError::NotFound(*digest),
                });
            }
            Layered {
                packs,
                rest: &Absent,
            }
            .read_blob(digest)
            .await
        })
    }

    fn read_stream<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<ReadStream>> {
        Box::pin(async move {
            let read = self.packs().read_large(digest, 0, digest.size_bytes);
            match read.await.map_err(unavailable)? {
                Some(windows) => Ok(windows.map(|window| window.map_err(unavailable)).boxed()),
                None => Err(StorageError::NotFound(*digest)),
            }
        })
    }

    fn read_many<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Vec<Answer<Bytes>>> {
        Box::pin(async move {
            Layered {
                packs: self.packs(),
                rest: &Absent,
            }
            .read_many(digests)
            .await
        })
    }

    fn packs(&self) -> Option<&Packs> {
        Some(self.stored.packs())
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        Box::pin(self.stored.action_result(action))
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        Box::pin(async move {
            let closure = checked(result, &self.stored.held()).await?;
            let encoded = result.encode_to_vec();
            Ok(ActionRecord {
                result: kbf_segments::sha256(&encoded),
                closure,
            })
        })
    }

    fn commit_action_record(
        &self,
        _action: Digest,
        _record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        Box::pin(futures::future::ready(Err(StorageError::Invalid(
            "a frontend with no storage node writes an action result whole \
             (put_action_result): a prepared record names a result blob it does not store"
                .to_owned(),
        ))))
    }

    fn put_action_result<'a>(
        &'a self,
        action: &'a Digest,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<()>> {
        Box::pin(self.stored.put_action_result(action, result))
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        Box::pin(async move {
            let layered = Layered {
                packs: self.packs(),
                rest: &Absent,
            };
            walk(&layered, root, &self.trees).await
        })
    }

    fn put_streamed<'a>(
        &'a self,
        digest: &'a Digest,
        open: &'a OpenPieces<'a>,
    ) -> BoxFuture<'a, Answer<()>> {
        Box::pin(async move {
            let pieces = || {
                open()
                    .map(|piece| {
                        piece.map_err(|e| {
                            tonic::Status::unavailable(format!("reading the blob: {e}"))
                        })
                    })
                    .boxed()
            };
            self.packs()
                .put_large(digest, pieces)
                .await
                .map_err(|status| match status.code() {
                    Code::InvalidArgument => StorageError::Invalid(status.message().to_owned()),
                    _ => unavailable(status),
                })
        })
    }

    fn metrics(&self) -> &CacheMetrics {
        &self.metrics
    }

    fn render_metrics(&self) -> String {
        let mut out = self.metrics.render();
        kbf_storage::in_flight::render(&mut out);
        out
    }

    fn leadership(&self) -> Option<Leadership> {
        None
    }
}

#[cfg(test)]
#[path = "stored_tests.rs"]
mod tests;
