//! The chain: who a write goes to under the map a node holds, and the call that carries
//! it to the replica.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::{Member, MemberId, MemberState};
use kbf_types::Digest;

use super::leg::Leg;
use super::peers::Peers;
use super::{MapSource, View, check};
use crate::StorageError;

/// The most owners the chain serves: a primary and one replica.
const MAX_REPLICATION: usize = 2;

/// How a chain waits on its replica. Hang bounds, not speed: a replica that takes
/// longer is judged gone, however fast the disk it is slow to.
#[derive(Clone, Copy, Debug)]
pub struct ChainConfig {
    /// The longest the replica may take to accept one message of the blob.
    pub stall: Duration,
    /// The longest the replica may take to verify and commit the blob once it has all of
    /// it.
    pub commit: Duration,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(30),
            commit: Duration::from_secs(120),
        }
    }
}

/// What this node does with a write it is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Stores the blob here and nowhere else: the only owner (R = 1), or a replica
    /// receiving the copy its primary sent.
    Local,
    /// The primary: stores the blob here and at `replica`, and answers only when both
    /// hold it.
    Primary { replica: Member },
}

/// There are fewer write owners than the map's replication factor.
#[derive(Debug, thiserror::Error)]
#[error("the map has {owners} write owners for this blob and a replication factor of {r}")]
struct TooFewOwners {
    owners: usize,
    r: usize,
}

/// The node holds a map but has not registered, so does not know which member it is.
#[derive(Debug, thiserror::Error)]
#[error("this node has not been told its member id (it has not registered)")]
struct NotRegistered;

/// A replica the map says is down, and so is not called.
#[derive(Debug, thiserror::Error)]
#[error("the replica {0} is down")]
struct ReplicaDown(MemberId);

/// R = 2 chain replication on one node: the map it judges by and the channels to its
/// peers.
pub struct Chain {
    source: Arc<dyn MapSource>,
    peers: Peers,
    config: ChainConfig,
}

impl Chain {
    /// A chain judging by `source`'s map and calling peers through `peers`.
    #[must_use]
    pub fn new(source: Arc<dyn MapSource>, peers: Peers, config: ChainConfig) -> Self {
        Self {
            source,
            peers,
            config,
        }
    }

    /// The view this node holds now, if it has a map and knows which member it is.
    pub(crate) fn current(&self) -> Option<View> {
        let map = self.source.current()?;
        let me = self.source.me()?;
        Some(View { map, me })
    }

    /// The map source, for a service that judges its other calls by it too.
    pub(crate) fn source(&self) -> Arc<dyn MapSource> {
        Arc::clone(&self.source)
    }

    /// The map a write routed by generation `requested` is judged by, with this node's
    /// member id.
    ///
    /// # Errors
    /// [`StorageError::StaleMap`], or [`StorageError::Unavailable`] for a caller ahead of
    /// the node, as [`crate::replicate`] says; [`StorageError::Unavailable`] if the node
    /// holds a map but has not been told which member it is.
    pub(crate) async fn view(
        &self,
        requested: u64,
        refetch_wait: Duration,
    ) -> Result<View, StorageError> {
        let map = check(&*self.source, requested, refetch_wait).await?;
        let me = self
            .source
            .me()
            .ok_or_else(|| StorageError::Unavailable(Arc::new(NotRegistered)))?;
        Ok(View { map, me })
    }

    /// What this node does with a write of `digest` under `view`, `leg_from` being the
    /// member that sent it as a replica's copy, if one did.
    ///
    /// # Errors
    /// [`StorageError::Invalid`] if this node is not where the map says the write goes: a
    /// client write to a node that is not the primary, a copy sent to a node that does not
    /// own the blob or by the node itself, or a replication factor above 2;
    /// [`StorageError::Unavailable`] if the map has fewer owners than its factor.
    pub(crate) fn plan(
        &self,
        view: &View,
        digest: &Digest,
        leg_from: Option<MemberId>,
    ) -> Result<Plan, StorageError> {
        let r = usize::from(view.map.r());
        if r > MAX_REPLICATION {
            return Err(StorageError::Invalid(format!(
                "the map's replication factor is {r}; chain replication serves {MAX_REPLICATION} \
                 at most"
            )));
        }
        let owners = view.map.write_owners(digest);
        if owners.len() < r {
            return Err(StorageError::Unavailable(Arc::new(TooFewOwners {
                owners: owners.len(),
                r,
            })));
        }
        let position = owners.iter().position(|owner| *owner == view.me);
        match (leg_from, position, owners.as_slice()) {
            (Some(sender), Some(_), _) if sender != view.me => Ok(Plan::Local),
            (Some(sender), _, _) => Err(StorageError::Invalid(format!(
                "{view_me} was sent a copy of {digest} by {sender}, and does not own it",
                view_me = view.me
            ))),
            (None, Some(0), [_, replica]) => {
                let replica = view
                    .map
                    .member(*replica)
                    .cloned()
                    .ok_or(StorageError::Internal("an owner is not in the map".into()))?;
                Ok(Plan::Primary { replica })
            }
            (None, Some(0), _) => Ok(Plan::Local),
            (None, _, _) => Err(StorageError::Invalid(format!(
                "{} is not the primary of {digest}",
                view.me
            ))),
        }
    }

    /// Whether the placement `plan` made for `digest` under `view` still holds: if the map
    /// turned while the blob was being written, the write is answered only when the new
    /// map places it the same way.
    ///
    /// # Errors
    /// [`StorageError::StaleMap`] naming the new generation if the placement changed.
    pub(crate) fn confirm(
        &self,
        view: &View,
        digest: &Digest,
        leg_from: Option<MemberId>,
        plan: &Plan,
    ) -> Result<(), StorageError> {
        match self.current() {
            Some(now) if now.map.generation() != view.map.generation() => {
                match self.plan(&now, digest, leg_from) {
                    Ok(ref placed) if placed == plan => Ok(()),
                    _ => Err(StorageError::StaleMap {
                        current: now.map.generation(),
                    }),
                }
            }
            _ => Ok(()),
        }
    }

    /// The call that carries a blob of `size` bytes to `replica`.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the map says `replica` is down;
    /// [`StorageError::Internal`] if it has no address a channel can be made to.
    pub(crate) fn open_leg(
        &self,
        view: &View,
        replica: &Member,
        size: u64,
    ) -> Result<Leg, StorageError> {
        if replica.state == MemberState::Down {
            return Err(StorageError::Unavailable(Arc::new(ReplicaDown(replica.id))));
        }
        let channel = self.peers.channel(&replica.addr)?;
        Ok(Leg::open(
            channel,
            view.map.generation(),
            view.me,
            size,
            self.config.stall,
            self.config.commit,
        ))
    }
}
