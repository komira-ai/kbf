//! The members as the update controller reads them: one look at the map and the rows, with
//! what the controller joins hosts to (the address a member registered) and what it gates
//! on (state, boot counter, the plan's mark, the newest report).

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_map::{Map, Member, MemberId, MemberState};

use super::{Members, MembershipError, down_key};
use crate::storage_health::{Debts, Writes};

/// One member, as [`Members::look`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberLook {
    /// The member.
    pub id: MemberId,
    /// The address it registered, which callers dial and a host is joined by.
    pub addr: String,
    /// Its state, from its row if it has one, else the map's.
    pub state: MemberState,
    /// Its boot counter: larger at every registration.
    pub boot: u64,
    /// Whether it is `Down` because an operator planned a restart and the plan holds.
    pub planned_down: bool,
    /// How long ago its newest report arrived, on the store's clock; `None` if it has none
    /// (it never reported, or its row expired).
    pub heard_ago: Option<Duration>,
    /// The copies it owes, if its newest report is fresh and says.
    pub repair_debt: Option<u64>,
    /// The writes it has open, if its newest report is fresh and says.
    pub open_writes: Option<u64>,
    /// The generation of the map it held when it made that report.
    pub reported_generation: Option<u64>,
    /// How long ago the `down/` mark of a planned restart was written; `None` without one.
    pub marked_ago: Option<Duration>,
}

/// Every member and the generation of the map they were read with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MembersLook {
    /// The generation of the current map; 0 before the first.
    pub generation: u64,
    /// The members, in id order.
    pub members: Vec<MemberLook>,
}

impl Members {
    /// Reads every member: the map's, with the live row's word for the ones that have a row.
    ///
    /// # Errors
    /// The store failing; a stored row not decoding.
    pub fn look(&self) -> Result<MembersLook, MembershipError> {
        let map = self.current_map()?;
        let rows = self.read_rows()?;
        let now = self.store.now();
        let mut known: BTreeMap<MemberId, Member> = BTreeMap::new();
        for member in map.as_ref().map_or(&[][..], Map::members) {
            known.insert(member.id, member.clone());
        }
        for (id, live) in &rows {
            known.insert(*id, live.member.clone());
        }
        let freshness = self.config.freshness();
        let mut members = Vec::with_capacity(known.len());
        for (id, member) in known {
            let live = rows.get(&id);
            let planned = if member.state == MemberState::Down {
                self.planned_of(id, live)?.in_force(now)
            } else {
                false
            };
            let report = live.and_then(|live| live.report);
            let fresh = report.filter(|report| now.since(report.at) <= freshness);
            let marked_ago = match self.store.get(&down_key(id))? {
                Some(mark) if planned => mark
                    .expires_at
                    .map(|end| self.config.dwell.saturating_sub(end.since(now))),
                _ => None,
            };
            members.push(MemberLook {
                id,
                addr: member.addr.clone(),
                state: member.state,
                boot: live.map_or(member.boot, |live| live.boot),
                planned_down: planned,
                heard_ago: report.map(|report| now.since(report.at)),
                repair_debt: fresh.and_then(|r| match r.said.debts {
                    Debts::Known(owed) => Some(owed),
                    Debts::Unknown => None,
                }),
                open_writes: fresh.and_then(|r| match r.said.writes {
                    Writes::Known(open) => Some(open),
                    Writes::Unknown => None,
                }),
                reported_generation: fresh.map(|r| r.said.generation),
                marked_ago,
            });
        }
        Ok(MembersLook {
            generation: map.as_ref().map_or(0, Map::generation),
            members,
        })
    }
}
