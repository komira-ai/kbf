//! FindMissing, split by owner.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::join_all;
use kbf_map::MemberId;
use kbf_storage::StorageError;
use kbf_types::Digest;

use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

impl Router {
    /// The digests among `digests` the farm does not hold, in the order asked (a digest
    /// asked twice is answered twice): each digest is asked of its primary owner, one
    /// call to each owner that is some digest's primary, all at once, and the answers
    /// are put together.
    ///
    /// Only the primary is asked. A cold build asks mostly about blobs nobody holds, and
    /// a replica asked on every miss would double the calls for nothing: whatever the
    /// primary has not got, a write sends again. When the primary cannot answer, its
    /// digests go to the next owner that is not down, so a call that completes was
    /// answered for every digest, never for some.
    ///
    /// # Errors
    /// Some digest's every owner is down or could not answer.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        let missing = self.routed(|view| self.find_in(view, digests)).await?;
        Ok(digests
            .iter()
            .filter(|d| missing.contains(d))
            .copied()
            .collect())
    }

    /// The digests missing, as the map `view` places them.
    async fn find_in(
        &self,
        view: MapView,
        digests: &[Digest],
    ) -> Result<BTreeSet<Digest>, StorageError> {
        let generation = view.map.generation();
        let mut missing = BTreeSet::new();
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
                node.find_missing(asked).await
            }))
            .await;
            for ((_, asked), answer) in by_owner.values().zip(answers) {
                match answer {
                    Ok(lacking) => {
                        missing.extend(lacking);
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
