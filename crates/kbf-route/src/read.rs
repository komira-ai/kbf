//! Reads: owners in rank order, then the previous generation's.

use std::future::Future;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::{FutureExt as _, StreamExt as _};
use kbf_map::MemberId;
use kbf_proto::reapi::ActionResult;
use kbf_storage::{ReadStream, Storage, StorageError};
use kbf_types::Digest;

use crate::resume::resumed;
use crate::router::{Router, read_order, unavailable};
use crate::source::MapView;

/// How one walk down the owners of a digest ended.
pub(crate) enum Walked<T> {
    /// An owner had it.
    Found(T),
    /// No owner did. `absent` of them said they do not hold it; `failure` is the first
    /// that could not say (down, or holding it unreadably).
    Missed {
        absent: usize,
        failure: Option<StorageError>,
    },
}

impl Router {
    /// The bytes `[offset, offset + limit)` of the blob `digest` names (to its end when
    /// `limit` is 0), as [`kbf_storage::Storage::read_range`] gives them, from the first
    /// owner that holds it: this generation's owners in rank order, then the members
    /// that may not hold it yet, then the previous generation's. Nothing is collected: a
    /// chunk is passed on as it arrives.
    ///
    /// An owner that fails before its first byte is skipped, and a read that fails after
    /// it goes on from the bytes already passed, on whichever owner answers
    /// ([`crate::MAX_RESUMES`] times at most), each owner checking the blob as a whole
    /// before it sends a byte, as the storage contract says.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] only if every owner said it does not hold the blob;
    /// [`StorageError::Unavailable`] if one could not say; [`StorageError::OutOfRange`]
    /// for an offset past the end.
    pub async fn read_range(
        &self,
        digest: &Digest,
        offset: u64,
        limit: u64,
    ) -> Result<ReadStream, StorageError> {
        let first = self.open(*digest, offset, limit).await?;
        Ok(resumed(self.clone(), *digest, offset, limit, first))
    }

    /// A whole blob, for the small ones a caller keeps in memory. Callers of one digest
    /// at the same time share one read, whichever owner answers it; the blob is checked
    /// against the length its digest names (every owner verifies the bytes before it
    /// sends them).
    ///
    /// # Errors
    /// As [`Router::read_range`].
    pub async fn read_blob(&self, digest: &Digest) -> Result<Bytes, StorageError> {
        let digest = *digest;
        let router = self.clone();
        self.flights
            .join(digest, move || {
                async move { router.collect(digest).await }.boxed()
            })
            .await
    }

    /// `GetActionResult`: the entry for `action`, and the digest of the blob it was read
    /// from, from the first owner that has one. An owner that does not is not the end: the
    /// next is asked, as for a blob. A miss is `None`; it is also what the call answers
    /// when some owners could not say and the others had no entry, since an action cache
    /// is only ever missed by it.
    ///
    /// # Errors
    /// Every owner failed.
    pub async fn action_result(
        &self,
        action: &Digest,
    ) -> Result<Option<(Digest, ActionResult)>, StorageError> {
        let action = *action;
        let walked = self
            .routed(|view| async move {
                self.walk(&view, &action, move |node| async move {
                    node.action_result(&action).await
                })
                .await
            })
            .await?;
        match walked {
            Walked::Found(entry) => Ok(Some(entry)),
            Walked::Missed {
                absent: 0,
                failure: Some(failure),
            } => Err(failure),
            Walked::Missed { .. } => Ok(None),
        }
    }

    /// The first stretch of a read: the owner that holds the blob answers.
    pub(crate) async fn open(
        &self,
        digest: Digest,
        offset: u64,
        limit: u64,
    ) -> Result<ReadStream, StorageError> {
        let walked = self
            .routed(|view| async move {
                self.walk(&view, &digest, move |node| async move {
                    match node.read_range(&digest, offset, limit).await {
                        Ok(stream) => Ok(Some(stream)),
                        Err(StorageError::NotFound(_)) => Ok(None),
                        Err(e) => Err(e),
                    }
                })
                .await
            })
            .await?;
        match walked {
            Walked::Found(stream) => Ok(stream),
            Walked::Missed {
                failure: Some(failure),
                ..
            } => Err(failure),
            Walked::Missed { failure: None, .. } => Err(StorageError::NotFound(digest)),
        }
    }

    /// The whole blob, in one piece.
    async fn collect(&self, digest: Digest) -> Result<Bytes, StorageError> {
        let mut stream = self.read_range(&digest, 0, 0).await?;
        let mut blob = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            blob.extend_from_slice(&chunk?);
        }
        if blob.len() as u64 != digest.size_bytes {
            return Err(StorageError::Internal(format!(
                "{digest} came back as {} bytes",
                blob.len()
            )));
        }
        Ok(blob.freeze())
    }

    /// Asks the owners of `digest` in read order until one has what `ask` is after.
    ///
    /// `ask` answers `Some` for what an owner holds and `None` for an owner that does
    /// not. A failure of an owner goes on to the next; a request that no other owner
    /// would answer differently (`Invalid`, `OutOfRange`) and `StaleMap`, which makes the
    /// whole order wrong, end the walk.
    pub(crate) async fn walk<T, Fut>(
        &self,
        view: &MapView,
        digest: &Digest,
        ask: impl Fn(Arc<dyn Storage>) -> Fut,
    ) -> Result<Walked<T>, StorageError>
    where
        Fut: Future<Output = Result<Option<T>, StorageError>>,
    {
        let order = read_order(view, digest);
        if order.is_empty() {
            return Err(unavailable(format!("every owner of {digest} is down")));
        }
        let generation = view.map.generation();
        let mut absent: Vec<MemberId> = Vec::new();
        let mut failure = None;
        for member in order {
            let node = match self.nodes.node(member, generation) {
                Ok(node) => node,
                Err(e) => {
                    failure.get_or_insert(e);
                    continue;
                }
            };
            match ask(node).await {
                Ok(Some(found)) => {
                    self.found_elsewhere(view, digest, member.id, &absent);
                    return Ok(Walked::Found(found));
                }
                Ok(None) => absent.push(member.id),
                Err(
                    e @ (StorageError::StaleMap { .. }
                    | StorageError::OutOfRange { .. }
                    | StorageError::Invalid(_)),
                ) => return Err(e),
                Err(e) => {
                    if matches!(e, StorageError::Unreachable(_)) {
                        absent.push(member.id);
                    }
                    failure.get_or_insert(e);
                }
            }
        }
        Ok(Walked::Missed {
            absent: absent.len(),
            failure,
        })
    }

    /// Tells the repair hook, if there is one and its limit allows, that `found_at`
    /// answered after owners of this generation `absent` did not. An owner that failed
    /// without saying it lacked the blob is not in `absent`: it is down, and av2-11 repairs
    /// what it missed when it is back.
    fn found_elsewhere(
        &self,
        view: &MapView,
        digest: &Digest,
        found_at: MemberId,
        absent: &[MemberId],
    ) {
        let Some(repair) = &self.repair else {
            return;
        };
        let owners = view.map.write_owners(digest);
        let missing: Vec<MemberId> = absent
            .iter()
            .copied()
            .filter(|id| owners.contains(id))
            .collect();
        if !missing.is_empty() && repair.limiter.allow() {
            repair.hook.found_elsewhere(digest, found_at, &missing);
        }
    }
}
