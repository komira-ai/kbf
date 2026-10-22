//! Rule 2 of `docs/design/storage-ram-ack.md`: a durable record that names a blob is
//! written only once the blob is on the disk of an owner. This is the question the action
//! cache asks before it commits an entry ([`Router::commit_with_closure`]).

use std::collections::{BTreeMap, BTreeSet};

use futures::future::join_all;
use kbf_map::{Member, MemberId};
use kbf_meta::ActionWriteError;
use kbf_storage::StorageError;
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_types::Digest;

use crate::find::Ask;
use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

impl Router {
    /// The digests among `digests` that no write owner of `view` answers present to `ask`:
    /// the primary is asked about all of them in one batched call (a call per primary), and
    /// each round after that asks the next owner about the digests still lacking, so a blob
    /// that is on a replica only is found, and a warm farm pays one call per primary.
    ///
    /// # Errors
    /// `STALE_MAP` from a node, for [`Router::routed`] to retry; the first failure of an
    /// owner that could not answer for a digest no owner that did answer holds: that is
    /// not "lacking", since the blob may be there.
    pub(crate) async fn lacking_at_every_owner(
        &self,
        view: &MapView,
        digests: &[Digest],
        ask: Ask,
    ) -> Result<BTreeSet<Digest>, StorageError> {
        let generation = view.map.generation();
        let mut pending: BTreeSet<Digest> = digests.iter().copied().collect();
        // Digests an owner could not answer for: undetermined, so not lacking.
        let mut undetermined: BTreeSet<Digest> = BTreeSet::new();
        let mut failure = None;
        for rank in 0.. {
            let mut by_owner: BTreeMap<MemberId, (&Member, Vec<Digest>)> = BTreeMap::new();
            for digest in &pending {
                if let Some(owner) = write_order(&view.map, digest).get(rank) {
                    by_owner
                        .entry(owner.id)
                        .or_insert_with(|| (owner, Vec::new()))
                        .1
                        .push(*digest);
                }
            }
            if by_owner.is_empty() {
                break;
            }
            let answers = join_all(by_owner.values().map(|(owner, asked)| async move {
                let node = self.nodes.node(owner, generation)?;
                // A node bounds the digests of one call, so a long list is several.
                let mut lacking = Vec::new();
                for chunk in asked.chunks(MAX_FIND_MISSING_DIGESTS) {
                    lacking.extend(ask.of(node.as_ref(), chunk).await?);
                }
                Ok::<_, StorageError>(lacking)
            }))
            .await;
            for ((_, asked), answer) in by_owner.values().zip(answers) {
                match answer {
                    Ok(lacking) => {
                        let lacking: BTreeSet<Digest> = lacking.into_iter().collect();
                        for digest in asked.iter().filter(|d| !lacking.contains(d)) {
                            pending.remove(digest);
                        }
                    }
                    Err(stale @ StorageError::StaleMap { .. }) => return Err(stale),
                    Err(e) => {
                        undetermined.extend(asked.iter().copied());
                        failure.get_or_insert(e);
                    }
                }
            }
        }
        match failure {
            Some(failure) if pending.iter().any(|d| undetermined.contains(d)) => Err(failure),
            _ => Ok(pending),
        }
    }

    /// Rule 2, barrier (a): every blob of `needed` is durable on a write owner, asked with
    /// `kbf-durable: wait` so a blob on its way to the disk is given time to get there.
    ///
    /// A blob no owner holds durably fails the call: [`ActionWriteError::Absent`] when no
    /// owner holds it at all, UNAVAILABLE when an owner holds it in RAM and it was not
    /// written within that owner's `--durable-wait` (the commit can be tried again). In the
    /// warm case this is one batched call per primary.
    ///
    /// # Errors
    /// As above; `STALE_MAP` from a node; an owner that could not answer.
    pub(crate) async fn require_durable(
        &self,
        view: &MapView,
        needed: &[Digest],
    ) -> Result<(), StorageError> {
        let lacking = self
            .lacking_at_every_owner(view, needed, Ask::DurableWaiting)
            .await?;
        if lacking.is_empty() {
            return Ok(());
        }
        let asked: Vec<Digest> = lacking.iter().copied().collect();
        let absent = self
            .lacking_at_every_owner(view, &asked, Ask::Servable)
            .await?;
        if !absent.is_empty() {
            // In the order the entry names them (the result first), each once.
            let mut seen = BTreeSet::new();
            let listed: Vec<Digest> = needed
                .iter()
                .filter(|d| absent.contains(d) && seen.insert(**d))
                .copied()
                .collect();
            return Err(StorageError::ActionWrite(ActionWriteError::Absent(listed)));
        }
        Err(unavailable(format!(
            "{} blob(s) of the entry are held in RAM by their owners and not yet on a disk, \
             first {}; the commit can be tried again",
            lacking.len(),
            asked[0]
        )))
    }
}
