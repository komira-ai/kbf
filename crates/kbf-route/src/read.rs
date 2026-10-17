//! Reads: owners in rank order, then the previous generation's.

use std::future::Future;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::{FutureExt as _, StreamExt as _};
use kbf_map::MemberId;
use kbf_proto::reapi::ActionResult;
use kbf_segments::RecordHasher;
use kbf_storage::{ReadStream, Storage, StorageError};
use kbf_types::Digest;

use crate::resume::resumed;
use crate::router::{Router, read_order, unavailable};
use crate::source::MapView;

/// How one walk down the owners of a digest ended.
pub(crate) enum Walked<T> {
    /// An owner had it.
    Found(T),
    /// No owner did. `absent` of them said they do not hold it, or hold it unreadably or
    /// wrong; `failure` is the first that could not say (down, or one of those).
    Missed {
        absent: usize,
        failure: Option<StorageError>,
    },
}

impl<T> Walked<T> {
    /// What the walk found, or why it found nothing: the failure of the first owner that
    /// could not say, else `NotFound` (every owner said it does not hold `digest`).
    fn found(self, digest: Digest) -> Result<T, StorageError> {
        match self {
            Self::Found(found) => Ok(found),
            Self::Missed {
                failure: Some(failure),
                ..
            } => Err(failure),
            Self::Missed { failure: None, .. } => Err(StorageError::NotFound(digest)),
        }
    }
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
    /// before it sends a byte, as the storage contract says. The router does not hash what
    /// passes through it: a caller that holds the digest checks the bytes it receives
    /// (a whole blob is [`Router::read_blob`]).
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
    /// at the same time share one read, whichever owner answers it.
    ///
    /// The bytes are hashed and compared with `digest` before they are returned, whatever
    /// the owner says it verified. An owner whose bytes do not hash to it is treated as one
    /// that failed: the next owner is asked, and the read-repair hook is told the copy is
    /// bad. An owner that breaks part way is the same, and the next owner is read from the
    /// start; a blob read whole is not resumed.
    ///
    /// # Errors
    /// As [`Router::read_range`]; [`StorageError::Corrupt`] when the first owner that
    /// could not say was one that sent bytes that are not the blob.
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
        walked.found(digest)
    }

    /// The whole blob, in one piece, from the first owner that sends the bytes it names.
    async fn collect(&self, digest: Digest) -> Result<Bytes, StorageError> {
        let walked = self
            .routed(|view| async move {
                self.walk(&view, &digest, move |node| whole(node, digest))
                    .await
            })
            .await?;
        walked.found(digest)
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
                    if matches!(
                        e,
                        StorageError::Unreachable(_) | StorageError::Corrupt { .. }
                    ) {
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

/// The blob `digest` names, read whole from `node` and checked against it: `None` if the
/// node does not hold it.
///
/// At most one byte past the blob's size is read, which is enough to know a node sends too
/// much; reading on would only fill memory with what is thrown away.
///
/// # Errors
/// [`StorageError::Corrupt`] if the bytes do not hash to `digest`; the node's own failure
/// if it fails before or while sending.
async fn whole(node: Arc<dyn Storage>, digest: Digest) -> Result<Option<Bytes>, StorageError> {
    let mut stream = match node.read_range(&digest, 0, 0).await {
        Ok(stream) => stream,
        Err(StorageError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let most = digest.size_bytes.saturating_add(1);
    let mut blob = BytesMut::new();
    let mut hasher = RecordHasher::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let room = most.saturating_sub(blob.len() as u64);
        let take = chunk.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        hasher.update(&chunk[..take]);
        blob.extend_from_slice(&chunk[..take]);
        if blob.len() as u64 == most {
            break;
        }
    }
    let (actual, _) = hasher.finish();
    if actual != digest {
        return Err(StorageError::Corrupt { digest, actual });
    }
    Ok(Some(blob.freeze()))
}
