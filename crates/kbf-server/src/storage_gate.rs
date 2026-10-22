//! G_S, the storage gate (`docs/design/split-mode-updates.md` section 2.7): may one
//! storage member go out, or come back in, without leaving a blob with one copy?
//!
//! "Never take both owners" cannot be checked per blob: under rendezvous hashing any two
//! members co-own some blob. So the rule is about the whole fleet. With the host's own
//! member out, every other member must be `Active`, heard within a time to live, free of
//! repair debt, and neither `CatchingUp` nor planned down; and at least one other member
//! must exist to hold the second copy. Every unknown fails: a member not heard, a debt
//! not reported, a member the facts do not list.
//!
//! The same module holds what the gate is asked over: [`MemberFacts`], the members as
//! [`member_facts`] reads them from a look, and [`storage_half`], which joins a node to its
//! member by the address it enrolled with. The operator API's safe-to-stop query asks all
//! of them, and the update controller reads the same ones.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_state::Store;
use kbf_types::{FarmTime, WorkerId};

use crate::members::{MemberLook, MembersLook};

/// What a host's storage half is, as `Members` holds it. One per storage member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberFacts {
    /// The member's id.
    pub id: MemberId,
    /// Its state in the map.
    pub state: MemberState,
    /// Whether the `down/<id>` mark of a planned restart exists. Read as a flag of its
    /// own, because the map can lag the mark: a member can still read `Active` after the
    /// mark was written (section 2.6, step 4).
    pub planned_down: bool,
    /// When the controller last heard it (a heartbeat); `None` if never or unknown.
    pub heard_at: Option<FarmTime>,
    /// The blobs it owes copies of (its repair journal); `None` if it does not report.
    pub repair_debt: Option<u64>,
    /// When the planned-down mark was written; `None` if there is no mark.
    pub down_since: Option<FarmTime>,
    /// Its boot counter as it registered (larger with every process start); `None` if it
    /// does not report one.
    pub boot: Option<u64>,
}

impl MemberFacts {
    /// Whether it was heard within `ttl` of `now`. A time in the future is not: it would
    /// make a row look fresh by a clock that is wrong.
    #[must_use]
    pub fn is_fresh(&self, now: FarmTime, ttl: Duration) -> bool {
        self.heard_at
            .is_some_and(|heard| heard <= now && now.saturating_duration_since(heard) <= ttl)
    }
}

/// Which question the gate answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateMode {
    /// Before a pick: the host's own member is `Active` and heard (it is not already
    /// leaving or dead), and the rest as for [`GateMode::Out`] without a freshness mark.
    Pick,
    /// With the host's member marked out (step 1, and step 5 after the mark): its own
    /// state is not asked, only its repair debt. With `fresh_since`, every other member
    /// must also have reported at or after that time, so the answer is about the fleet
    /// as it is with the node out, not before.
    Out {
        /// The time the node went out; `None` asks only for a report within the time to live.
        fresh_since: Option<FarmTime>,
    },
    /// With the host's member back (step 8): every member, its own too, as for a pick.
    In,
}

/// Why the gate is shut. The first thing found, the worst first: a second member down
/// before a debt before a report not yet in.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateFail {
    /// The host has no storage member in the facts: not linked, or not listed.
    #[error("its storage member is not linked, or the facts do not list it")]
    Unlinked,
    /// No other member exists to hold the second copy.
    #[error("no other member holds the second copy")]
    NoPeer,
    /// A member has a planned-down mark.
    #[error("member {member} is planned down")]
    PlannedDown {
        /// The member.
        member: MemberId,
    },
    /// A member is rebuilding its index; its reads answer `UNAVAILABLE`.
    #[error("member {member} is catching up")]
    CatchingUp {
        /// The member.
        member: MemberId,
    },
    /// A member is in a state other than `Active` (`Down`, `Draining`, `Joining`).
    #[error("member {member} is {state:?}")]
    NotActive {
        /// The member.
        member: MemberId,
        /// Its state.
        state: MemberState,
    },
    /// A member was not heard within the time to live: it counts as down.
    #[error("member {member} was not heard within the time to live")]
    Silent {
        /// The member.
        member: MemberId,
    },
    /// A member has not reported since the node went out, so the answer would be about the
    /// fleet before. Not a failure yet: the report is due within a heartbeat.
    #[error("member {member} has not reported since the node went out")]
    NotReportedSince {
        /// The member.
        member: MemberId,
    },
    /// A member does not report its repair debt: unknown is not zero.
    #[error("member {member} does not report its repair debt")]
    DebtUnknown {
        /// The member.
        member: MemberId,
    },
    /// A member owes copies, so the other copy of something is missing.
    #[error("member {member} owes {blobs} blobs")]
    Debt {
        /// The member.
        member: MemberId,
        /// How many.
        blobs: u64,
    },
}

impl GateFail {
    /// Whether a member other than the host is `Down` or silent: the case in which the
    /// run holds, because blobs owned by both are `UNAVAILABLE` until one returns (section
    /// 5.2). Every other failure only gives the host back.
    #[must_use]
    pub const fn is_second_member_down(&self) -> bool {
        matches!(
            self,
            Self::NotActive {
                state: MemberState::Down,
                ..
            } | Self::Silent { .. }
        )
    }

    /// Whether waiting a heartbeat may open the gate: the report it needs is not in yet.
    #[must_use]
    pub const fn is_report_pending(&self) -> bool {
        matches!(self, Self::NotReportedSince { .. })
    }

    /// 2: a second member down; 0: a report not yet in; 1: anything else.
    const fn rank(&self) -> u8 {
        if self.is_second_member_down() {
            2
        } else if self.is_report_pending() {
            0
        } else {
            1
        }
    }
}

/// Evaluates G_S for the storage member `own` (a member of `members`, or the gate is
/// shut with [`GateFail::Unlinked`]).
///
/// # Errors
/// The worst thing that keeps the gate shut ([`GateFail`]); the first of equals, members
/// in id order.
pub fn storage_gate(
    members: &[MemberFacts],
    own: MemberId,
    mode: GateMode,
    ttl: Duration,
    now: FarmTime,
) -> Result<(), GateFail> {
    let me = members
        .iter()
        .find(|m| m.id == own)
        .ok_or(GateFail::Unlinked)?;
    let mut worst: Option<GateFail> = None;
    let mut note = |fail: GateFail| {
        if worst.as_ref().is_none_or(|w| fail.rank() > w.rank()) {
            worst = Some(fail);
        }
    };
    if !matches!(mode, GateMode::Out { .. })
        && let Err(fail) = live(me, ttl, now)
    {
        note(fail);
    }
    // The debt it owes counts whatever the mode: a node going out with copies owed leaves
    // them owed.
    if let Err(fail) = debt_settled(me) {
        note(fail);
    }
    let mut peers: Vec<&MemberFacts> = members.iter().filter(|m| m.id != own).collect();
    peers.sort_by_key(|m| m.id);
    if peers.is_empty() {
        note(GateFail::NoPeer);
    }
    for peer in peers {
        if let Err(fail) = live(peer, ttl, now) {
            note(fail);
            continue;
        }
        if let GateMode::Out {
            fresh_since: Some(since),
        } = mode
            && peer.heard_at.is_none_or(|heard| heard < since)
        {
            note(GateFail::NotReportedSince { member: peer.id });
            continue;
        }
        if let Err(fail) = debt_settled(peer) {
            note(fail);
        }
    }
    worst.map_or(Ok(()), Err)
}

/// `member` is `Active`, not planned down, and heard within the time to live.
fn live(member: &MemberFacts, ttl: Duration, now: FarmTime) -> Result<(), GateFail> {
    let id = member.id;
    if member.planned_down {
        return Err(GateFail::PlannedDown { member: id });
    }
    match member.state {
        MemberState::Active => {}
        MemberState::CatchingUp => return Err(GateFail::CatchingUp { member: id }),
        state => return Err(GateFail::NotActive { member: id, state }),
    }
    if member.is_fresh(now, ttl) {
        Ok(())
    } else {
        Err(GateFail::Silent { member: id })
    }
}

/// `member` reports a repair debt of zero. An absent report is not zero.
fn debt_settled(member: &MemberFacts) -> Result<(), GateFail> {
    match member.repair_debt {
        Some(0) => Ok(()),
        Some(blobs) => Err(GateFail::Debt {
            member: member.id,
            blobs,
        }),
        None => Err(GateFail::DebtUnknown { member: member.id }),
    }
}

/// How a Linux host stands to the storage members.
pub(crate) enum Link<'a> {
    /// The registry knows the node and it advertises no address: no storage half.
    NoStorage,
    /// Its advertised address is the one `member` registered.
    Member(&'a MemberLook),
    /// No member is its own; the text says why.
    Unlinked(String),
}

/// The member whose registered address is the address `host` advertised when it enrolled
/// (`adverts`: node id to address), both read through [`kbf_enroll_registry::canonical_addr`],
/// the one spelling enrollment stores and a member registers. No prefix, no port guessing: an
/// address that is not a member's links nothing.
pub(crate) fn link<'a>(
    host: &WorkerId,
    adverts: &BTreeMap<String, String>,
    members: &'a MembersLook,
) -> Link<'a> {
    let Some(advertised) = adverts.get(host.as_str()) else {
        return Link::Unlinked(format!(
            "{host} is not in the enrollment registry, so its storage address is not known"
        ));
    };
    if advertised.is_empty() {
        return Link::NoStorage;
    }
    let advertised = &kbf_enroll_registry::canonical_addr(advertised);
    let mut matching = members
        .members
        .iter()
        .filter(|m| kbf_enroll_registry::canonical_addr(&m.addr) == *advertised);
    match (matching.next(), matching.next()) {
        (Some(only), None) => Link::Member(only),
        (None, _) => Link::Unlinked(format!(
            "{host} advertises {advertised}, which no storage member registered"
        )),
        (Some(_), Some(_)) => Link::Unlinked(format!(
            "{host} advertises {advertised}, which more than one storage member registered"
        )),
    }
}

/// The address each enrolled node advertised (node id to address; empty for a node with no
/// storage role), from the enrollment registry in the store.
pub(crate) fn advertised(store: &dyn Store) -> Result<BTreeMap<String, String>, String> {
    let nodes = kbf_enroll_registry::list_nodes(store)
        .map_err(|e| format!("the enrollment registry could not be read: {e}"))?;
    Ok(nodes
        .into_iter()
        .map(|node| (node.id.to_string(), node.advertise))
        .collect())
}

/// The storage member that is the storage half of the host `host`, joined by the address it
/// enrolled with as the update controller joins it; `None` if it has no
/// storage half.
/// The operator API's safe-to-stop query asks it of a node id (`crate::api`).
///
/// # Errors
/// Why the host cannot be told from a daemon-only host: the registry could not be read, does
/// not know it, or its address is no member's or more than one member's.
pub fn storage_half(
    store: &dyn Store,
    host: &WorkerId,
    members: &MembersLook,
) -> Result<Option<MemberLook>, String> {
    match link(host, &advertised(store)?, members) {
        Link::NoStorage => Ok(None),
        Link::Member(member) => Ok(Some(member.clone())),
        Link::Unlinked(why) => Err(why),
    }
}

/// `now` less `ago`.
fn before(now: FarmTime, ago: std::time::Duration) -> FarmTime {
    let ms = u64::try_from(ago.as_millis()).unwrap_or(u64::MAX);
    FarmTime::from_millis(now.as_millis().saturating_sub(ms))
}

/// The members of `look` as the gate reads them, ages taken from `now`. The operator API's
/// safe-to-stop query reads them the same way (`crate::api`).
#[must_use]
pub fn member_facts(look: &MembersLook, now: FarmTime) -> Vec<MemberFacts> {
    look.members
        .iter()
        .map(|m| MemberFacts {
            id: m.id,
            state: m.state,
            planned_down: m.planned_down,
            heard_at: m.heard_ago.map(|ago| before(now, ago)),
            repair_debt: m.repair_debt,
            down_since: m.marked_ago.map(|ago| before(now, ago)),
            boot: (m.boot > 0).then_some(m.boot),
        })
        .collect()
}
