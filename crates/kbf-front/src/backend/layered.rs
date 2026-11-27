//! The blobs Execute reads, with the frontend's packs looked in first.
//!
//! A frontend given [`Packs`] writes what clients upload into packs, so the Action, the
//! Command and the input tree that Execute reads are in the pack index and not on the storage
//! nodes. [`Layered`] asks the packs for a blob and, if they do not serve it, the blobs under
//! it (a node, or the nodes of a map); for what is held it asks the index first and the nodes
//! about only what the index lacks. A read through it says how long its index lookup and its
//! ranged read of a pack took ([`Blobs::read_timed`]), which the input walk reports. A store
//! without packs does not build one: its calls are the ones they were. The closure check of the action cache reads the blobs through it too
//! (`closed`, and the proxy's `GetActionResult`), so a result's output that is only in a pack
//! is held.

use std::sync::Arc;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_pack::ReadTimes;
use kbf_route::Router;
use kbf_storage::StorageError;
use kbf_types::Digest;
use tonic::Status;

use super::Answer;
use crate::input_walk::Blobs;
use crate::proxy::Packs;

/// A refusal of the packs as the store's: it could not answer.
pub(super) fn unavailable(status: Status) -> StorageError {
    StorageError::Unavailable(Arc::new(status))
}

/// What one loss of the packs moves a [`Layered`] epoch by: odd, so distinct counts of losses
/// are distinct epochs.
const LOSS_EPOCH_STEP: u64 = 0x9E37_79B9_7F4A_7C15;

/// `packs` first, then `rest`.
pub(crate) struct Layered<'a> {
    pub packs: &'a Packs,
    pub rest: &'a dyn Blobs,
}

impl Layered<'_> {
    /// The blobs `digests`, one answer each in the order asked, as [`Blobs::read_blob`]
    /// answers one: the packs are asked for all of them together (one index lookup, and blobs
    /// that lie close in a pack are one ranged read), and what they do not serve is read from
    /// `rest`, one by one at once.
    pub(crate) async fn read_many(&self, digests: &[Digest]) -> Vec<Answer<Bytes>> {
        let packed = self.packs.read_many(digests).await;
        futures::future::join_all(
            digests
                .iter()
                .zip(packed)
                .map(|(digest, packed)| async move {
                    match packed {
                        Ok(Some(bytes)) => Ok(bytes),
                        Ok(None) => self.rest.read_blob(digest).await,
                        Err(status) => Err(unavailable(status)),
                    }
                }),
        )
        .await
    }
}

impl Blobs for Layered<'_> {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(async move { self.read_timed(digest).await.0 })
    }

    fn read_timed<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, (Answer<Bytes>, ReadTimes)> {
        Box::pin(async move {
            let (packed, times) = self.packs.read_timed(digest).await;
            let answer = match packed.map_err(unavailable) {
                Ok(Some(bytes)) => Ok(bytes),
                Ok(None) => self.rest.read_blob(digest).await,
                Err(error) => Err(error),
            };
            (answer, times)
        })
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move {
            let absent = self.packs.missing(digests).await.map_err(unavailable)?;
            if absent.is_empty() {
                return Ok(absent);
            }
            self.rest.find_missing(&absent).await
        })
    }

    /// The epoch of `rest`, moved on by every loss the packs have found ([`Packs::losses`]):
    /// a "held" answer rests on a row of the pack index as much as on `rest`, and a row goes
    /// only after a read or a check through the packs found its blob lost or damaged. `None`
    /// when `rest` cannot say. Every count of losses is a distinct epoch over one epoch of
    /// `rest` (an odd multiplier is one to one).
    fn epoch(&self) -> Option<u64> {
        let losses = self.packs.losses().wrapping_mul(LOSS_EPOCH_STEP);
        self.rest.epoch().map(|epoch| epoch ^ losses)
    }
}

/// The router as the store's own `find_missing` and `read_blob` ask it: a digest is asked of
/// its primary, not of every owner a read would try (which is how a walk of the inputs asks
/// it).
pub(crate) struct Primary<'a>(pub &'a Router);

impl Blobs for Primary<'_> {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(Router::read_blob(self.0, digest))
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(Router::find_missing(self.0, digests))
    }
}
