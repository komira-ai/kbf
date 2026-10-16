//! The map, its members and its owner functions.

use std::collections::BTreeSet;
use std::fmt;

use kbf_types::Digest;

use crate::hrw;

/// A member's identity, assigned by the state store at registration (no node picks its
/// own). Ordered, so that equal scores break the same way everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberId(pub u64);

impl fmt::Display for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "m{}", self.0)
    }
}

/// Where a member is in its life; the table in the crate documentation says what each
/// state owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberState {
    /// Admitted; a write owner at once, read last.
    Joining,
    /// Rebuilding its index or receiving copies; as `Joining`.
    CatchingUp,
    /// Caught up: a full owner.
    Active,
    /// Leaving: takes no new writes, still read in rank order until its successor has
    /// the data.
    Draining,
    /// Lease lapsed, kept in the map for the dwell.
    Down,
}

impl MemberState {
    /// Whether the member can be a write owner.
    #[must_use]
    pub const fn takes_writes(self) -> bool {
        !matches!(self, Self::Draining)
    }

    /// Whether the member is among the settled owners: it holds the data placed before
    /// any join or drain in progress.
    const fn is_settled(self) -> bool {
        matches!(self, Self::Active | Self::Draining | Self::Down)
    }
}

/// How owners are chosen from the scores. One rule exists; zone-aware placement is a
/// later addition (`architecture-v2.md` section 6); the enum is exhaustive, so a new rule
/// fails to compile where the wire conversion and the owner functions match on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// The top `r` members by weighted rendezvous score.
    Rendezvous,
}

/// One storage node as the map holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// The member's id.
    pub id: MemberId,
    /// The address callers dial (non-empty).
    pub addr: String,
    /// The zone label; empty when the node has none.
    pub zone: String,
    /// Its share of the digests, relative to the others (at least 1).
    pub weight: u32,
    /// Its state.
    pub state: MemberState,
}

/// Why a map was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// Generation 0 means "no map seen" on the wire.
    #[error("generation 0 is reserved for \"no map seen\"")]
    ZeroGeneration,
    /// A replication factor of 0 places nothing.
    #[error("replication factor 0")]
    ZeroReplication,
    /// More owners wanted than members.
    #[error("replication factor {r} exceeds the {members} members")]
    ReplicationExceedsMembers {
        /// The factor asked for.
        r: u8,
        /// The members in the map.
        members: usize,
    },
    /// Two members share an id.
    #[error("member id {0} appears twice")]
    DuplicateMember(MemberId),
    /// A member with weight 0 would own nothing.
    #[error("member {0} has weight 0")]
    ZeroWeight(MemberId),
    /// A member with no address cannot be dialled.
    #[error("member {0} has no address")]
    EmptyAddr(MemberId),
    /// A name twice in the deny list.
    #[error("deny list names {0:?} twice")]
    DuplicateDeny(String),
}

/// The storage map of one generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    generation: u64,
    members: Vec<Member>,
    rule: Rule,
    r: u8,
    min_protocol: u32,
    deny_list: Vec<String>,
}

impl Map {
    /// Builds a map, refusing one that could not place a digest.
    ///
    /// `deny_list` names the instance ids the frontend refuses to register.
    pub fn new(
        generation: u64,
        members: Vec<Member>,
        rule: Rule,
        r: u8,
        min_protocol: u32,
        deny_list: Vec<String>,
    ) -> Result<Self, MapError> {
        if generation == 0 {
            return Err(MapError::ZeroGeneration);
        }
        if r == 0 {
            return Err(MapError::ZeroReplication);
        }
        let mut ids = BTreeSet::new();
        for m in &members {
            if !ids.insert(m.id) {
                return Err(MapError::DuplicateMember(m.id));
            }
            if m.weight == 0 {
                return Err(MapError::ZeroWeight(m.id));
            }
            if m.addr.is_empty() {
                return Err(MapError::EmptyAddr(m.id));
            }
        }
        if usize::from(r) > members.len() {
            return Err(MapError::ReplicationExceedsMembers {
                r,
                members: members.len(),
            });
        }
        let mut denied = BTreeSet::new();
        if let Some(twice) = deny_list.iter().find(|name| !denied.insert(name.as_str())) {
            return Err(MapError::DuplicateDeny(twice.clone()));
        }
        Ok(Self {
            generation,
            members,
            rule,
            r,
            min_protocol,
            deny_list,
        })
    }

    /// The generation; later maps have larger numbers.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The members, in the order the map was built with.
    #[must_use]
    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// The placement rule.
    #[must_use]
    pub fn rule(&self) -> Rule {
        self.rule
    }

    /// The replication factor.
    #[must_use]
    pub fn r(&self) -> u8 {
        self.r
    }

    /// The lowest protocol version a member may speak.
    #[must_use]
    pub fn min_protocol(&self) -> u32 {
        self.min_protocol
    }

    /// The instance ids refused at registration.
    #[must_use]
    pub fn deny_list(&self) -> &[String] {
        &self.deny_list
    }

    /// The member with this id.
    #[must_use]
    pub fn member(&self, id: MemberId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// Every member, best score first for this digest; equal scores go to the lower id.
    fn ranked(&self, digest: &Digest) -> Vec<&Member> {
        let Rule::Rendezvous = self.rule;
        let mut scored: Vec<(f64, &Member)> = self
            .members
            .iter()
            .map(|m| (hrw::score(digest, m.id, m.weight), m))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
        scored.into_iter().map(|(_, m)| m).collect()
    }

    /// Who a write of this digest goes to, primary (rank 1) first: the top `r` among the
    /// members whose state takes writes. A `Down` member keeps its slot, so the caller
    /// checks [`Member::state`] and records a repair; a `Draining` one never appears.
    /// Fewer than `r` come back only when too few members take writes.
    #[must_use]
    pub fn write_owners(&self, digest: &Digest) -> Vec<MemberId> {
        self.ranked(digest)
            .into_iter()
            .filter(|m| m.state.takes_writes())
            .take(usize::from(self.r))
            .map(|m| m.id)
            .collect()
    }

    /// The order to try members when reading this digest in this generation: the top `r`
    /// settled owners in rank order, then the write owners not yet listed (the members
    /// that may not hold it yet, `Joining` and `CatchingUp` among them). `Down` members
    /// are left out.
    #[must_use]
    pub fn read_order(&self, digest: &Digest) -> Vec<MemberId> {
        let ranked = self.ranked(digest);
        let r = usize::from(self.r);
        let settled = ranked.iter().filter(|m| m.state.is_settled()).take(r);
        let writers = ranked.iter().filter(|m| m.state.takes_writes()).take(r);
        let mut order = Vec::new();
        for m in settled
            .chain(writers)
            .filter(|m| m.state != MemberState::Down)
        {
            if !order.contains(&m.id) {
                order.push(m.id);
            }
        }
        order
    }

    /// [`Map::read_order`], then the previous generation's read order without the members
    /// already listed. Ids from `previous` may name members this map no longer has; look
    /// those up in `previous`.
    #[must_use]
    pub fn read_order_with_previous(
        &self,
        previous: Option<&Map>,
        digest: &Digest,
    ) -> Vec<MemberId> {
        let mut order = self.read_order(digest);
        for id in previous.map_or_else(Vec::new, |p| p.read_order(digest)) {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        order
    }
}
