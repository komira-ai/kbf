//! The map, its members and its owner functions.

use std::collections::BTreeSet;
use std::fmt;

use kbf_types::Digest;

use crate::deny::DenyEntry;
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
    /// A state this build does not name, with the number it came with (a later build wrote
    /// it; 0 is a member that named none). The member is inert: it takes no writes, is not a
    /// settled owner, is never listed for reading and is not counted as `Active`. It stays
    /// in the map, and a row or map that is written back says the number it came with.
    Unknown(i32),
}

impl MemberState {
    /// The state as an operator reads it: the lower-case, underscored name `GET
    /// /v1/storage` shows. Exhaustive: a new state fails to compile here instead of
    /// reaching operators unnamed.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Joining => "joining",
            Self::CatchingUp => "catching_up",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Down => "down",
            Self::Unknown(_) => "unknown",
        }
    }

    /// Whether the member can be a write owner.
    #[must_use]
    pub const fn takes_writes(self) -> bool {
        !matches!(self, Self::Draining | Self::Unknown(_))
    }

    /// Whether the member is among the settled owners: it holds the data placed before
    /// any join or drain in progress.
    const fn is_settled(self) -> bool {
        matches!(self, Self::Active | Self::Draining | Self::Down)
    }
}

/// How owners are chosen from the scores. One rule is named; zone-aware placement is a
/// later addition (`architecture-v2.md` section 6). A rule a later build writes reads as
/// [`Rule::Unknown`], which places nothing; the owner functions match on the enum, so a new
/// rule fails to compile where they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// The top `r` members by weighted rendezvous score.
    ///
    /// This value fixes the whole computation: the SipHash-2-4 keys, the byte layout of
    /// the hashed key (digest hash, size, member id), the reading of the hash as `u`, the
    /// score `weight / neg_ln(u)` and `neg_ln` itself, down to the last bit. Any change to
    /// one of them is a NEW rule value (and a new wire value), never an edit of this one,
    /// because it remaps every blob in the farm.
    Rendezvous,
    /// A rule this build does not name, with the number it came with (0 is a map that named
    /// none). The map is read and everything else in it applies (members, replication, the
    /// protocol floor, the deny list), but this build cannot place with a rule it does not
    /// know, so [`Map::write_owners`] and [`Map::read_order`] list no one.
    Unknown(i32),
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
    /// The boot counter of the registration this entry stands for: larger at every
    /// registration of the member, so a restart inside the lease changes the entry (and
    /// the map's generation) and a cache keyed by it drops what the old boot held. 0 is
    /// "unknown": a map from a frontend that predates the field.
    pub boot: u64,
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
    /// The same deny entry twice, after canonical spelling.
    #[error("deny entry \"{0}\" appears twice")]
    DuplicateDenied(DenyEntry),
}

/// The storage map of one generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    generation: u64,
    members: Vec<Member>,
    rule: Rule,
    r: u8,
    min_protocol: u32,
    denied: Vec<DenyEntry>,
}

impl Map {
    /// Builds a map, refusing one that could not place a digest.
    ///
    /// `denied` lists what the frontend refuses: certificate serials, public key hashes and
    /// node ids (not member ids, which are assigned at registration, after the certificate
    /// is checked). Entries are canonical already ([`DenyEntry`]), so a repeat of one is
    /// refused whatever its original spelling.
    pub fn new(
        generation: u64,
        members: Vec<Member>,
        rule: Rule,
        r: u8,
        min_protocol: u32,
        denied: Vec<DenyEntry>,
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
        let mut seen = BTreeSet::new();
        if let Some(twice) = denied.iter().find(|entry| !seen.insert(*entry)) {
            return Err(MapError::DuplicateDenied(twice.clone()));
        }
        Ok(Self {
            generation,
            members,
            rule,
            r,
            min_protocol,
            denied,
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

    /// What the frontend refuses.
    #[must_use]
    pub fn denied(&self) -> &[DenyEntry] {
        &self.denied
    }

    /// The member with this id.
    #[must_use]
    pub fn member(&self, id: MemberId) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// Every member, best score first for this digest; equal scores go to the lower id.
    /// None under a rule this build does not name.
    fn ranked(&self, digest: &Digest) -> Vec<&Member> {
        let Rule::Rendezvous = self.rule else {
            return Vec::new();
        };
        let mut scored: Vec<(f64, &Member)> = self
            .members
            .iter()
            .map(|m| (hrw::score(digest, m.id, m.weight), m))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.id.cmp(&b.1.id)));
        scored.into_iter().map(|(_, m)| m).collect()
    }

    /// Who a write of this digest goes to, primary (rank 1) first.
    ///
    /// The owners are the top `r` by score among the members whose state takes writes,
    /// listed settled members first (best score first), then the joiners (`Joining`,
    /// `CatchingUp`). A joiner takes writes at once but is never rank 1 while a settled
    /// writer exists, because rank 1 is the head of the chain, the one flusher and the
    /// `FindMissing` target, and a joiner holds nothing yet: if the top `r` are all
    /// joiners, the best settled writer takes the place of the lowest-scored joiner. When
    /// no settled writer exists at all (a fresh farm), the joiners are the owners and the
    /// best-scored one is primary; answering nobody would stall the first writes. With
    /// `r` = 1 the only slot is rank 1, so a joiner owns nothing while a settled writer
    /// exists; its slice reaches it by the catch-up copy.
    ///
    /// A `Down` member keeps its slot, so the caller checks [`Member::state`] and records
    /// a repair; with `r` = 1 and the only owner `Down` the caller refuses the write with
    /// `UNAVAILABLE`, as there is no second copy to make. A `Draining` member never
    /// appears, nor does a member in a state this build does not name
    /// ([`MemberState::Unknown`]). Fewer than `r` come back only when too few members take
    /// writes, and none under a rule this build does not name ([`Rule::Unknown`]).
    #[must_use]
    pub fn write_owners(&self, digest: &Digest) -> Vec<MemberId> {
        self.write_members(digest)
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    fn write_members(&self, digest: &Digest) -> Vec<&Member> {
        let writers: Vec<&Member> = self
            .ranked(digest)
            .into_iter()
            .filter(|m| m.state.takes_writes())
            .collect();
        let mut owners: Vec<&Member> = writers.iter().take(usize::from(self.r)).copied().collect();
        if !owners.iter().any(|m| m.state.is_settled())
            && let Some(settled) = writers.iter().find(|m| m.state.is_settled())
        {
            owners.pop();
            owners.push(settled);
        }
        // Stable: settled members first, each group in score order.
        owners.sort_by_key(|m| !m.state.is_settled());
        owners
    }

    /// The order to try members when reading this digest in this generation: the top `r`
    /// settled owners in rank order, then the write owners not yet listed (the members
    /// that may not hold it yet, `Joining` and `CatchingUp` among them, last). `Down`
    /// members are left out: with every owner `Down` the list is empty and the caller
    /// treats the blob as unavailable (`UNAVAILABLE`), not as missing. So are members in a
    /// state this build does not name, and every member under a rule it does not name.
    #[must_use]
    pub fn read_order(&self, digest: &Digest) -> Vec<MemberId> {
        self.read_members(digest)
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    fn read_members(&self, digest: &Digest) -> Vec<&Member> {
        let r = usize::from(self.r);
        let settled: Vec<&Member> = self
            .ranked(digest)
            .into_iter()
            .filter(|m| m.state.is_settled())
            .take(r)
            .collect();
        let mut order: Vec<&Member> = Vec::new();
        for m in settled
            .into_iter()
            .chain(self.write_members(digest))
            .filter(|m| m.state != MemberState::Down)
        {
            if !order.iter().any(|seen| seen.id == m.id) {
                order.push(m);
            }
        }
        order
    }

    /// [`Map::read_order`], then the previous generation's read order without the members
    /// already listed. The members come back as the map that holds them gives them: one
    /// that left between the generations is the `previous` map's `Member`, with the
    /// address it had there.
    #[must_use]
    pub fn read_order_with_previous<'a>(
        &'a self,
        previous: Option<&'a Map>,
        digest: &Digest,
    ) -> Vec<&'a Member> {
        let mut order = self.read_members(digest);
        for m in previous.map_or_else(Vec::new, |p| p.read_members(digest)) {
            if !order.iter().any(|seen| seen.id == m.id) {
                order.push(m);
            }
        }
        order
    }

    /// A copy of this map in which member `id` is in `state`, and everything else is as it
    /// is (same generation, members, weights and replication); `None` if `id` is not a
    /// member.
    ///
    /// It answers "what would placement be if this member were promoted": a `Joining`
    /// member assumed `Active` is a settled owner, so the copy's owner lists, read orders
    /// and [`Map::placement_fingerprint`] are those of the map after the promotion. The
    /// copy is for computing, never for publishing: it carries the original generation.
    #[must_use]
    pub fn assuming(&self, id: MemberId, state: MemberState) -> Option<Map> {
        let mut copy = self.clone();
        copy.members.iter_mut().find(|m| m.id == id)?.state = state;
        Some(copy)
    }

    /// The write owners of `digest` ([`Map::write_owners`]) if member `id` were in
    /// `state`; `None` if `id` is not a member.
    ///
    /// For a `Joining` member assumed `Active` this is what the member will own once
    /// promoted: with equal weights it owns about `r / N` of the digests and is rank 1 for
    /// about `1 / N`, where as `Joining` it is never rank 1 (and at `r` = 1 owns nothing).
    /// Assumed `Draining` it owns nothing. Costs one map clone; for many digests call
    /// [`Map::assuming`] once.
    #[must_use]
    pub fn owners_assuming(
        &self,
        id: MemberId,
        state: MemberState,
        digest: &Digest,
    ) -> Option<Vec<MemberId>> {
        self.assuming(id, state).map(|m| m.write_owners(digest))
    }

    /// The read order of `digest` ([`Map::read_order`]) if member `id` were in `state`;
    /// `None` if `id` is not a member. A joiner assumed `Active` is read in rank order
    /// among the settled owners instead of last.
    #[must_use]
    pub fn read_order_assuming(
        &self,
        id: MemberId,
        state: MemberState,
        digest: &Digest,
    ) -> Option<Vec<MemberId>> {
        self.assuming(id, state).map(|m| m.read_order(digest))
    }

    /// A 64-bit fingerprint of everything that decides which members own a digest, and
    /// nothing else, so that a report made against one map can be checked against another:
    /// equal fingerprints mean equal [`Map::write_owners`] and [`Map::read_order`] for every
    /// digest, and a different one means some digest moved.
    ///
    /// It covers the rule, the replication factor and, for each member in id order, the
    /// id, weight and the member's *placement class*. `Joining` and `CatchingUp`
    /// are one class (placement treats them alike); `Active`, `Draining` and `Down` are
    /// each their own. A member in a state this build does not name is left out, as
    /// placement leaves it out. Under a rule this build does not name nothing is placed, so
    /// the fingerprint is that of the rule alone. It leaves out the generation, addresses,
    /// boot counters, zones, the protocol floor, the deny list and the order the members
    /// were listed in, none of which move a digest. Zone is left out because placement does not read it today (ranking
    /// scores only digest, id and weight); when zone-aware placement exists it is a new
    /// layout, and the fingerprint (a new function, see below) adds zone then.
    ///
    /// The value is SipHash-2-4 with fixed keys over a fixed byte layout (see the code), so
    /// it is the same in every build and process. Changing the layout is a new function,
    /// not an edit of this one: processes of different versions compare these values.
    #[must_use]
    pub fn placement_fingerprint(&self) -> u64 {
        use std::hash::Hasher;

        use siphasher::sip::SipHasher24;

        let mut h = SipHasher24::new_with_keys(
            u64::from_be_bytes(*b"kbf-plfp"),
            u64::from_be_bytes(*b"placemnt"),
        );
        let Rule::Rendezvous = self.rule else {
            h.write_u8(0);
            return h.finish();
        };
        h.write_u8(1); // rule: Rendezvous
        h.write_u8(self.r);
        let mut members: Vec<(&Member, u8)> = self
            .members
            .iter()
            .filter_map(|m| {
                let class = match m.state {
                    MemberState::Joining | MemberState::CatchingUp => 0,
                    MemberState::Active => 1,
                    MemberState::Draining => 2,
                    MemberState::Down => 3,
                    MemberState::Unknown(_) => return None,
                };
                Some((m, class))
            })
            .collect();
        members.sort_by_key(|(m, _)| m.id);
        h.write_u64(members.len() as u64);
        for (m, class) in members {
            h.write_u64(m.id.0);
            h.write_u32(m.weight);
            h.write_u8(class);
        }
        h.finish()
    }

    /// The read order across this generation and the retained earlier ones: this map's
    /// [`Map::read_order`] first, then each map of `history` in the order given (newest
    /// first), each contributing its read order without the members already listed. A
    /// member is listed once, as the earliest map that has it gives it (the `Member` with
    /// the address it had there), so a blob written several generations back is found
    /// through a member that has since left. With an empty `history` this is
    /// [`Map::read_order`]; with one map it is [`Map::read_order_with_previous`].
    #[must_use]
    pub fn read_order_with_history<'a>(
        &'a self,
        history: &[&'a Map],
        digest: &Digest,
    ) -> Vec<&'a Member> {
        let mut order = self.read_members(digest);
        for map in history {
            for m in map.read_members(digest) {
                if !order.iter().any(|seen| seen.id == m.id) {
                    order.push(m);
                }
            }
        }
        order
    }
}
