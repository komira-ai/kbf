//! FindMissing, split by owner.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::join_all;
use kbf_map::MemberId;
use kbf_storage::StorageError;
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_types::Digest;

use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

/// Which question a FindMissing round asks of a node.
#[derive(Clone, Copy)]
enum Ask {
    /// Blobs it can serve, RAM-held ones included ([`kbf_storage::Storage::find_missing`]).
    Servable,
    /// Blobs on its disk only ([`kbf_storage::Storage::find_durable`]).
    Durable,
}

impl Router {
    /// The digests among `digests` the farm does not hold, in the order asked (a digest
    /// asked twice is answered twice): each digest is asked of its primary owner, one
    /// call to each owner that is some digest's primary, all at once, and the answers
    /// are put together.
    ///
    /// A node takes at most [`MAX_FIND_MISSING_DIGESTS`] digests in one call, so an owner
    /// with more is asked in several, one after the other.
    ///
    /// Only the primary is asked. A cold build asks mostly about blobs nobody holds, and
    /// a replica asked on every miss would double the calls for nothing: whatever the
    /// primary has not got, a write sends again. This is the question a client's
    /// FindMissing is: a blob only a replica holds is reported missing, the client
    /// uploads it, and the upload fills the primary. A caller that must not refuse a
    /// blob a read would find (the input check of an Execute) asks
    /// [`Router::find_missing_anywhere`]. When the primary cannot answer, its
    /// digests go to the next owner that is not down, so a call that completes was
    /// answered for every digest, never for some.
    ///
    /// # Errors
    /// Some digest's every owner is down or could not answer.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        let missing = self
            .routed(|view| async move { self.find_in(&view, digests).await })
            .await?;
        Ok(digests
            .iter()
            .filter(|d| missing.contains_key(d))
            .copied()
            .collect())
    }

    /// [`Router::find_missing`] counting only blobs durable on the owner asked: one held
    /// in RAM and not yet on its disk is missing (`kbf-durable: only`,
    /// [`kbf_storage::Storage::find_durable`]). Asked of the same owners in the same order,
    /// so a digest is answered by its primary, or by the next owner when the primary
    /// cannot answer. The question rule 2 asks before a durable record names a blob.
    ///
    /// # Errors
    /// As [`Router::find_missing`].
    pub async fn find_durable(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        let missing = self
            .routed(|view| async move { self.find_in_as(&view, digests, Ask::Durable).await })
            .await?;
        Ok(digests
            .iter()
            .filter(|d| missing.contains_key(d))
            .copied()
            .collect())
    }

    /// The digests missing, as the map `view` places them, each with the owner that said
    /// it lacks it.
    pub(crate) async fn find_in(
        &self,
        view: &MapView,
        digests: &[Digest],
    ) -> Result<BTreeMap<Digest, MemberId>, StorageError> {
        self.find_in_as(view, digests, Ask::Servable).await
    }

    /// [`Router::find_in`], asking the question `ask` names.
    async fn find_in_as(
        &self,
        view: &MapView,
        digests: &[Digest],
        ask: Ask,
    ) -> Result<BTreeMap<Digest, MemberId>, StorageError> {
        let generation = view.map.generation();
        let mut missing = BTreeMap::new();
        let mut pending: BTreeSet<Digest> = digests.iter().copied().collect();
        let mut failure = None;
        // Each round asks every pending digest of the owner at the next rank.
        for rank in 0.. {
            if pending.is_empty() {
                break;
            }
            let mut by_owner: BTreeMap<MemberId, (&kbf_map::Member, Vec<Digest>)> = BTreeMap::new();
            for digest in &pending {
                let owners = write_order(&view.map, digest);
                let Some(owner) = owners.get(rank) else {
                    return Err(failure.unwrap_or_else(|| {
                        unavailable(format!("every owner of {digest} is down"))
                    }));
                };
                by_owner
                    .entry(owner.id)
                    .or_insert_with(|| (owner, Vec::new()))
                    .1
                    .push(*digest);
            }
            let answers = join_all(by_owner.values().map(|(owner, asked)| async move {
                let node = self.nodes.node(owner, generation)?;
                // A node bounds the digests of one call, so a long list is several.
                let mut lacking = Vec::new();
                for chunk in asked.chunks(MAX_FIND_MISSING_DIGESTS) {
                    lacking.extend(match ask {
                        Ask::Servable => node.find_missing(chunk).await?,
                        Ask::Durable => node.find_durable(chunk).await?,
                    });
                }
                Ok(lacking)
            }))
            .await;
            for ((owner, asked), answer) in by_owner.values().zip(answers) {
                match answer {
                    Ok(lacking) => {
                        missing.extend(lacking.into_iter().map(|d| (d, owner.id)));
                        for digest in asked {
                            pending.remove(digest);
                        }
                    }
                    Err(stale @ StorageError::StaleMap { .. }) => return Err(stale),
                    Err(e) => {
                        failure.get_or_insert(e);
                    }
                }
            }
        }
        Ok(missing)
    }
}
