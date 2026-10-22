//! `kbf.map.v1` and the pure `kbf_map` types, both ways.
//!
//! Decoding skips what it does not know. A member state or a placement rule this build does
//! not name (or `UNSPECIFIED`) reads as `MemberState::Unknown` or `Rule::Unknown`, which
//! `kbf_map` treats as inert, and is written back as the number it came with. A replication
//! factor that is not a `u8`, a denied entry that is not `kind value`, or a map
//! `kbf_map::Map::new` refuses is an error, never a default.

use kbf_map::{DenyEntry, DenyEntryError, Map, MapError, Member, MemberId, MemberState, Rule};

use crate::kbf::map::v1 as pb;

/// Why a wire map was not read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MapDecodeError {
    /// A replication factor above 255.
    #[error("replication factor {0} is above 255")]
    Replication(u32),
    /// A denied entry that is not `kind value` (see `kbf_map::DenyEntry`).
    #[error("denied entry {entry:?}: {source}")]
    Denied {
        /// The string as it came.
        entry: String,
        /// What is wrong with it.
        source: DenyEntryError,
    },
    /// The map is well formed on the wire and refused by the domain.
    #[error("invalid map: {0}")]
    Invalid(#[from] MapError),
}

/// The number a state goes on the wire as: its own, or the one an unknown state came with.
fn state_to_wire(s: MemberState) -> i32 {
    match s {
        MemberState::Joining => i32::from(pb::MemberState::Joining),
        MemberState::CatchingUp => i32::from(pb::MemberState::CatchingUp),
        MemberState::Active => i32::from(pb::MemberState::Active),
        MemberState::Draining => i32::from(pb::MemberState::Draining),
        MemberState::Down => i32::from(pb::MemberState::Down),
        MemberState::Unknown(v) => v,
    }
}

/// The state a number names; one this build does not name (and 0) is `Unknown`, not an error.
fn state_from_wire(v: i32) -> MemberState {
    match pb::MemberState::try_from(v) {
        Ok(pb::MemberState::Joining) => MemberState::Joining,
        Ok(pb::MemberState::CatchingUp) => MemberState::CatchingUp,
        Ok(pb::MemberState::Active) => MemberState::Active,
        Ok(pb::MemberState::Draining) => MemberState::Draining,
        Ok(pb::MemberState::Down) => MemberState::Down,
        Ok(pb::MemberState::Unspecified) | Err(_) => MemberState::Unknown(v),
    }
}

impl From<&Member> for pb::Member {
    fn from(m: &Member) -> Self {
        Self {
            id: m.id.0,
            addr: m.addr.clone(),
            zone: m.zone.clone(),
            weight: m.weight,
            state: state_to_wire(m.state),
            boot: m.boot,
        }
    }
}

impl From<pb::Member> for Member {
    fn from(m: pb::Member) -> Self {
        Self {
            id: MemberId(m.id),
            addr: m.addr,
            zone: m.zone,
            weight: m.weight,
            state: state_from_wire(m.state),
            boot: m.boot,
        }
    }
}

impl From<&Map> for pb::Map {
    fn from(m: &Map) -> Self {
        let rule = match m.rule() {
            Rule::Rendezvous => i32::from(pb::Rule::Rendezvous),
            Rule::Unknown(v) => v,
        };
        Self {
            generation: m.generation(),
            members: m.members().iter().map(pb::Member::from).collect(),
            rule,
            replication: u32::from(m.r()),
            min_protocol: m.min_protocol(),
            denied: m.denied().iter().map(ToString::to_string).collect(),
        }
    }
}

impl TryFrom<pb::Map> for Map {
    type Error = MapDecodeError;

    fn try_from(m: pb::Map) -> Result<Self, MapDecodeError> {
        let rule = match pb::Rule::try_from(m.rule) {
            Ok(pb::Rule::Rendezvous) => Rule::Rendezvous,
            Ok(pb::Rule::Unspecified) | Err(_) => Rule::Unknown(m.rule),
        };
        let r =
            u8::try_from(m.replication).map_err(|_| MapDecodeError::Replication(m.replication))?;
        let members: Vec<Member> = m.members.into_iter().map(Member::from).collect();
        let denied = m
            .denied
            .into_iter()
            .map(|entry| {
                entry
                    .parse::<DenyEntry>()
                    .map_err(|source| MapDecodeError::Denied { entry, source })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Map::new(
            m.generation,
            members,
            rule,
            r,
            m.min_protocol,
            denied,
        )?)
    }
}
