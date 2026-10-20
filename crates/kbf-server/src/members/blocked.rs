//! Why a joiner is not promoted: [`Members::blocked_on`], the operator's answer
//! (`GET /v1/members`, `blocked_on`) and the tests' (`docs/design/rebalance-handoff.md`,
//! section 6). It judges as the sweeper ([`super::fence`]) and the joiner's beat
//! ([`super::promote`]) do, from the same functions, but only reads.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_map::{Map, MemberId, MemberState};
use kbf_state::StoreTime;
use prost::Message as _;

use super::handoff::{ADOPTING, Block, Unfilled};
use super::join::UNFILLED;
use super::row::Live;
use super::{MAP_KEY, Members, MembershipError, Progress, decode_map};

impl Members {
    /// What keeps joiner `id` from being promoted automatically now, in the order the
    /// conditions are met: empty for a member that is not a joiner, without
    /// [`Members::with_handoff`] (an operator is the only way then), or when only the next
    /// step of the sweeper or the joiner's next beat is owed. The joiner's rebuild is not
    /// judged: its row does not say.
    ///
    /// # Errors
    /// The store failing; a stored value that does not decode.
    pub fn blocked_on(&self, id: MemberId) -> Result<Vec<Block>, MembershipError> {
        Ok(self.blocked_joiners()?.remove(&id).unwrap_or_default())
    }

    /// [`Members::blocked_on`] of every joiner that has a mark and a row, by id.
    ///
    /// # Errors
    /// As [`Members::blocked_on`].
    pub fn blocked_joiners(&self) -> Result<BTreeMap<MemberId, Vec<Block>>, MembershipError> {
        let mut blocked = BTreeMap::new();
        let Some(handoff) = self.handoff else {
            return Ok(blocked);
        };
        let Some(map) = self
            .store
            .get(MAP_KEY)?
            .as_ref()
            .map(decode_map)
            .transpose()?
        else {
            return Ok(blocked);
        };
        let rows = self.read_rows()?;
        let now = self.store.now();
        for (key, mark) in self.store.scan_prefix(UNFILLED)? {
            let Some(id) = key
                .strip_prefix(UNFILLED)
                .and_then(|id| id.parse::<u64>().ok())
                .map(MemberId)
            else {
                continue;
            };
            let (Some(live), Ok(body)) = (rows.get(&id), Unfilled::decode(mark.value.as_slice()))
            else {
                continue;
            };
            if map.member(id).map(|m| m.state) != Some(MemberState::Joining) {
                continue;
            }
            let blocks = if body.phase == ADOPTING {
                self.adopting_blocks(&body, live, &map, now, handoff.settle)
            } else {
                // The barrier is judged against the generation the sweeper will date the
                // mark with, until it has.
                let need = if body.generation == 0 {
                    map.generation()
                } else {
                    body.generation
                };
                self.flush_blocks(&map, &rows, now, need)
            };
            blocked.insert(id, blocks);
        }
        Ok(blocked)
    }

    /// The blocks of a joiner whose fence is open, the settle time included.
    fn adopting_blocks(
        &self,
        body: &Unfilled,
        live: &Live,
        map: &Map,
        now: StoreTime,
        settle: Duration,
    ) -> Vec<Block> {
        let mut blocks = self.beat_blocks(body, live, Progress::CaughtUp, map);
        if blocks.is_empty() {
            let waited = if body.settled_nanos == 0 {
                Duration::ZERO
            } else {
                now.since(StoreTime::from_duration(Duration::from_nanos(
                    body.settled_nanos,
                )))
            };
            if waited < settle {
                blocks.push(Block::Settling(settle - waited));
            }
        }
        blocks
    }
}
