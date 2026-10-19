//! FindMissing as a read would answer it: a blob is missing only if no owner a read would
//! try holds it.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::join_all;
use kbf_map::{Member, MemberId};
use kbf_storage::StorageError;
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_types::Digest;

use crate::router::{Router, read_order};
use crate::source::MapView;

/// How many missing digests (with who was asked about each) a warning names.
const LOGGED: usize = 5;

impl Router {
    /// The digests among `digests` that no owner a read would try holds, in the order
    /// asked (a digest asked twice is answered twice).
    ///
    /// [`Router::find_missing`] asks the primary alone, so a primary that lacks a blob its
    /// replicas hold (a node that has just joined and is not yet filled, or whose copy was
    /// lost) says "missing" about a blob every read finds. A caller that refuses work for
    /// a missing blob, like the input check of an Execute, must not: this asks the primary
    /// first, then, for the digests it lacks, the rest of the read order of the map (this
    /// generation's owners, then the previous generation's while a change settles), a
    /// round per rank, one batched call per owner. A warm farm pays only the primary's
    /// call, as for [`Router::find_missing`].
    ///
    /// A blob found at an owner other than its primary is told to read repair
    /// ([`Router::with_read_repair`], [`Router::with_background_repair`]) with the owners
    /// of this generation that said they lack it; the call does not wait for the repair.
    ///
    /// A blob no owner holds is logged at warn (how many, the first few, and the owners
    /// asked about each).
    ///
    /// # Errors
    /// Some digest's every owner is down, or a digest that no owner that answered holds
    /// has an owner that could not answer: that is not "missing", since the blob may be
    /// there.
    pub async fn find_missing_anywhere(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Digest>, StorageError> {
        let gone = self
            .routed(|view| async move { self.anywhere_in(&view, digests).await })
            .await?;
        Ok(digests
            .iter()
            .filter(|d| gone.contains(d))
            .copied()
            .collect())
    }

    async fn anywhere_in(
        &self,
        view: &MapView,
        digests: &[Digest],
    ) -> Result<BTreeSet<Digest>, StorageError> {
        let said_no = self.find_in(view, digests).await?;
        let generation = view.map.generation();
        // The owners still to ask of each digest, in the order a read tries them; those
        // that said no, in the order they did.
        let mut next: BTreeMap<Digest, Vec<&Member>> = BTreeMap::new();
        let mut lacking: BTreeMap<Digest, Vec<MemberId>> = BTreeMap::new();
        for (digest, owner) in &said_no {
            let rest = read_order(view, digest)
                .into_iter()
                .filter(|m| m.id != *owner)
                .collect();
            next.insert(*digest, rest);
            lacking.insert(*digest, vec![*owner]);
        }
        let mut pending: BTreeSet<Digest> = said_no.keys().copied().collect();
        let mut found: BTreeMap<Digest, MemberId> = BTreeMap::new();
        // Digests an owner could not answer for: undetermined, so not gone.
        let mut undetermined: BTreeSet<Digest> = BTreeSet::new();
        let mut failure = None;
        for rank in 0.. {
            let mut by_owner: BTreeMap<MemberId, (&Member, Vec<Digest>)> = BTreeMap::new();
            for digest in &pending {
                if let Some(owner) = next[digest].get(rank) {
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
                let mut lacked = Vec::new();
                for chunk in asked.chunks(MAX_FIND_MISSING_DIGESTS) {
                    lacked.extend(node.find_missing(chunk).await?);
                }
                Ok::<_, StorageError>(lacked)
            }))
            .await;
            for ((owner, asked), answer) in by_owner.values().zip(answers) {
                match answer {
                    Ok(lacked) => {
                        let lacked: BTreeSet<Digest> = lacked.into_iter().collect();
                        for digest in asked {
                            if lacked.contains(digest) {
                                lacking.entry(*digest).or_default().push(owner.id);
                            } else {
                                found.insert(*digest, owner.id);
                                pending.remove(digest);
                            }
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
        for (digest, at) in &found {
            self.found_elsewhere(view, digest, *at, &lacking[digest]);
        }
        if let Some(failure) = failure.filter(|_| pending.iter().any(|d| undetermined.contains(d)))
        {
            return Err(failure);
        }
        if !pending.is_empty() {
            let named: Vec<String> = pending
                .iter()
                .take(LOGGED)
                .map(|d| {
                    let asked: Vec<String> = lacking[d].iter().map(ToString::to_string).collect();
                    format!("{d} (asked {})", asked.join(","))
                })
                .collect();
            tracing::warn!(
                generation,
                count = pending.len(),
                "blob(s) no owner holds, in this map or the previous one: {}",
                named.join("; ")
            );
        }
        Ok(pending)
    }
}
