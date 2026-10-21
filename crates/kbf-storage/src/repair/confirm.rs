//! Peer confirmation (`docs/design/storage-ram-ack.md`, "Peer confirmation and the repair
//! debt"): a node that has acknowledged a blob from RAM keeps its digest in an in-memory
//! set until the other owner is known to have the blob on its disk. If that owner crashed
//! in between, this node holds the only copy and owes nothing, which is wrong: the debt is
//! the thing the repair worker pays.
//!
//! [`PeerConfirmer::track`] puts a digest in the set; [`PeerConfirmer::drained`] says this
//! node has written it, which starts the grace ([`RepairConfig::peer_grace`]);
//! [`PeerConfirmer::confirm`] asks each peer, once per batch and with `kbf-durable: only`,
//! about the digests whose grace is over. A peer that has the blob durably: forgotten, and
//! nothing was written. A peer that has not, or could not answer, is the case the debt is
//! for: [`Repairs::owe`] (the existing durable journal, off the acknowledgement path), and
//! the worker copies the blob when the peer is back. So the normal path does no IO, and a
//! restarted peer (which answers missing) is paid a debt.
//!
//! The set is capped ([`RepairConfig::unconfirmed_cap`]): a digest that arrives when it is
//! full journals its debt at once. **Nothing puts a digest here yet**: the acknowledgement
//! point that does is a later change, so the set is empty and a pass asks nobody.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use kbf_map::{MemberId, MemberState};
use kbf_types::Digest;
use tokio::time::Instant;

use super::{RepairConfig, RepairRecord, Repairs};
use crate::replicate::Chain;
use crate::service::{MAX_FIND_MISSING_DIGESTS, RemoteStorage};
use crate::{Storage, StorageError};

/// What [`PeerConfirmer::track`] did with a digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracked {
    /// It is in the set, to be confirmed.
    Waiting,
    /// The set was full, so the debt was journalled at once.
    Owed,
}

/// What one [`PeerConfirmer::confirm`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Confirmed {
    /// Digests the peer holds on its disk: forgotten.
    pub durable: usize,
    /// Digests the peer did not hold on its disk, or could not be asked about: owed.
    pub owed: usize,
    /// Digests that are no longer this node's to confirm (it is not an owner now, or there
    /// is no other owner): forgotten.
    pub dropped: usize,
}

/// One digest the set keeps.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// The owner that was to confirm, and the generation the write was acknowledged under:
    /// what a debt would name.
    replica: MemberId,
    generation: u64,
    /// When the peer is asked: after this node's own write and the grace. `None` until
    /// [`PeerConfirmer::drained`].
    due: Option<Instant>,
}

/// The blobs this node acknowledged from RAM whose other owner has not been seen to hold
/// them on its disk.
pub struct PeerConfirmer {
    chain: Arc<Chain>,
    repairs: Arc<Repairs>,
    grace: Duration,
    probe: Duration,
    cap: usize,
    unconfirmed: Mutex<BTreeMap<Digest, Entry>>,
}

impl PeerConfirmer {
    /// A confirmer for the node `chain` is a link of, journalling its debts in `repairs`,
    /// paced by `config` (`peer_grace`, `probe` and `unconfirmed_cap`).
    #[must_use]
    pub fn new(chain: Arc<Chain>, repairs: Arc<Repairs>, config: &RepairConfig) -> Self {
        Self {
            chain,
            repairs,
            grace: config.peer_grace,
            probe: config.probe,
            cap: config.unconfirmed_cap,
            unconfirmed: Mutex::default(),
        }
    }

    fn set(&self) -> MutexGuard<'_, BTreeMap<Digest, Entry>> {
        self.unconfirmed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// How many digests are waiting to be confirmed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.set().len()
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set().is_empty()
    }

    /// Whether `digest` is waiting.
    #[must_use]
    pub fn is_tracked(&self, digest: &Digest) -> bool {
        self.set().contains_key(digest)
    }

    /// Keeps `digest`, acknowledged under map generation `generation` with `replica` as
    /// the owner to confirm. A digest already kept keeps its first entry. When the set is
    /// full the debt is journalled at once instead.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the set was full and the journal could not make the
    /// debt durable: nothing is owed or kept, and the caller must not rely on a
    /// confirmation.
    pub async fn track(
        &self,
        digest: Digest,
        replica: MemberId,
        generation: u64,
    ) -> Result<Tracked, StorageError> {
        {
            let mut set = self.set();
            if set.contains_key(&digest) {
                return Ok(Tracked::Waiting);
            }
            if set.len() < self.cap {
                set.insert(
                    digest,
                    Entry {
                        replica,
                        generation,
                        due: None,
                    },
                );
                return Ok(Tracked::Waiting);
            }
        }
        drop(
            self.repairs
                .owe(RepairRecord {
                    digest,
                    replica,
                    generation,
                })
                .await?,
        );
        Ok(Tracked::Owed)
    }

    /// Says this node has written `digest` to its own disk at `now`: the peer is asked
    /// once the grace is over. A digest that is not kept is ignored.
    pub fn drained(&self, digest: &Digest, now: Instant) {
        if let Some(entry) = self.set().get_mut(digest) {
            entry.due = Some(now + self.grace);
        }
    }

    /// One confirmation: every digest whose grace is over at `now` is asked of the other
    /// owner, in batches of at most [`MAX_FIND_MISSING_DIGESTS`], one call per batch with
    /// `kbf-durable: only`, and forgotten. A digest the peer holds on its disk is
    /// forgotten with nothing written; any other is owed ([`Repairs::owe`]).
    ///
    /// A peer the map names down, or that does not answer within
    /// [`RepairConfig::probe`], or whose answer is an error, is not known to hold the blob:
    /// its digests are owed, and the worker settles the debt (without a copy, if the peer
    /// turns out to hold it) when it can ask.
    pub async fn confirm(&self, now: Instant) -> Confirmed {
        let due: Vec<(Digest, Entry)> = self
            .set()
            .iter()
            .filter(|(_, entry)| entry.due.is_some_and(|due| due <= now))
            .map(|(digest, entry)| (*digest, *entry))
            .collect();
        if due.is_empty() {
            return Confirmed::default();
        }
        let view = self.chain.current();
        let mut confirmed = Confirmed::default();
        // The digests of each peer, with what a debt for them would say.
        let mut by_peer: BTreeMap<MemberId, Vec<(Digest, Entry)>> = BTreeMap::new();
        for (digest, entry) in due {
            let peer = view.as_ref().and_then(|view| {
                view.map
                    .write_owners(&digest)
                    .into_iter()
                    .find(|id| *id != view.me)
            });
            match peer {
                Some(peer) => by_peer.entry(peer).or_default().push((digest, entry)),
                None if view.is_some() => {
                    self.set().remove(&digest);
                    confirmed.dropped += 1;
                }
                // No map: nothing can be asked, and what is owed is for the journal.
                None => by_peer
                    .entry(entry.replica)
                    .or_default()
                    .push((digest, entry)),
            }
        }
        let answers = join_all(
            by_peer
                .iter()
                .map(|(peer, entries)| self.ask(view.as_ref(), *peer, entries)),
        )
        .await;
        for (entries, durable) in by_peer.values().zip(answers) {
            for (digest, entry) in entries {
                if durable.contains(digest) {
                    confirmed.durable += 1;
                } else if self.owe(*digest, *entry).await {
                    confirmed.owed += 1;
                } else {
                    // The journal could not take the debt: asked again at the next pass.
                    continue;
                }
                self.set().remove(digest);
            }
        }
        confirmed
    }

    /// The digests among `entries` that `peer` holds on its disk; none when it could not
    /// be asked.
    async fn ask(
        &self,
        view: Option<&crate::replicate::View>,
        peer: MemberId,
        entries: &[(Digest, Entry)],
    ) -> Vec<Digest> {
        let Some(remote) = self.dial(view, peer) else {
            return Vec::new();
        };
        let mut durable = Vec::new();
        for batch in entries.chunks(MAX_FIND_MISSING_DIGESTS) {
            let digests: Vec<Digest> = batch.iter().map(|(digest, _)| *digest).collect();
            let asked = tokio::time::timeout(self.probe, remote.find_durable(&digests)).await;
            let Ok(Ok(missing)) = asked else {
                // Not answered: not known durable, for any digest of the batch.
                continue;
            };
            durable.extend(digests.into_iter().filter(|d| !missing.contains(d)));
        }
        durable
    }

    /// A client of `peer`; `None` when there is no map, the map does not name the peer or
    /// names it down, or it cannot be dialled: then it is not known to hold anything.
    fn dial(&self, view: Option<&crate::replicate::View>, peer: MemberId) -> Option<RemoteStorage> {
        let view = view?;
        let member = view.map.member(peer)?;
        if member.state == MemberState::Down {
            return None;
        }
        let channel = self.chain.peer(&member.addr).ok()?;
        Some(RemoteStorage::new(channel, view.map.generation()))
    }

    /// Journals the debt for `digest`; whether it is durable now.
    async fn owe(&self, digest: Digest, entry: Entry) -> bool {
        let record = RepairRecord {
            digest,
            replica: entry.replica,
            generation: entry.generation,
        };
        match self.repairs.owe(record).await {
            Ok(pending) => {
                drop(pending);
                true
            }
            Err(e) => {
                tracing::warn!(%digest, %e, "a blob acknowledged from RAM could not be journalled as owed to its other owner");
                false
            }
        }
    }
}
