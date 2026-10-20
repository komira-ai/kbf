//! The sweeper's side of a joiner's fill (`docs/design/rebalance-handoff.md`, section 4):
//! [`Members::advance`], which [`Members::refresh`] runs after every map change and on the
//! timer, moves each `unfilled/<id>` mark of a `Joining` member along:
//!
//! 1. **Dating.** A mark that is empty (as a frontend without this wrote it) is dated with
//!    the generation of the map that first held the joiner: the barrier of the next step.
//! 2. **The flush barrier.** When every member in the map that is not `Down`, whatever its
//!    state (the joiner and any second joiner included: a node that is not an owner flushes
//!    what it stages, so a second joiner can hold another owner's blobs), reports a fresh
//!    `flush_clear_gen` at or above that generation, everything staged before the join is
//!    flushed to the cold store. A member that cannot say blocks, and so does a `Down`
//!    member still inside its dwell, which also marks the join *degraded*.
//! 3. **The fence.** The frontend then opens a tail epoch (`meta/tail_epoch`, one counter
//!    for the farm), and records in the mark the epoch, the placement fingerprint with the
//!    joiner assumed `Active`, and the boot of every write owner. Nodes learn the epoch
//!    from their heartbeat's answer and acknowledge it ([`Fill::tail_epoch_done`]) when a
//!    tail pass that began after learning it has finished. No clock is in the ordering.
//! 4. **Liveness of the fence.** If the fingerprint now differs from the mark's (the
//!    placement changed), or a write owner's boot differs from the recorded one (a restart,
//!    or a registration after the fence), a report made under the old one proves nothing:
//!    the frontend opens a newer epoch and records the present values. An owner that was
//!    recorded and has left the map marks the join degraded.
//!
//! The promotion is the joiner's own beat's ([`super::promote`]). With no [`super::Handoff`]
//! none of this runs and the marks stay as they were written.

use std::collections::BTreeMap;

use kbf_map::{Map, MemberId, MemberState};
use kbf_state::{Cond, StoreTime, Txn};
use prost::Message as _;

use super::handoff::{ADOPTING, Block, FLUSHING, FenceBoot, Unfilled, fill_fp};
use super::join::{UNFILLED, unfilled_key};
use super::row::Live;
use super::{MAP_KEY, Members, MembershipError, decode_map};

/// The counter of tail epochs: eight bytes, big endian; absent is 0.
pub(super) const TAIL_EPOCH: &str = "meta/tail_epoch";

/// Steps of one mark in one [`Members::advance`]: dating, the fence, a newer epoch.
const STEPS: usize = 4;

/// The boot of every member of `map` that takes writes, by member id.
pub(super) fn write_owner_boots(map: &Map) -> Vec<FenceBoot> {
    let mut boots: Vec<FenceBoot> = map
        .members()
        .iter()
        .filter(|m| m.state.takes_writes())
        .map(|m| FenceBoot {
            member: m.id.0,
            boot: m.boot,
        })
        .collect();
    boots.sort_by_key(|b| b.member);
    boots
}

impl Members {
    /// The tail epoch the farm is at: what a heartbeat's answer tells the nodes to
    /// acknowledge. 0 before the first fence opened.
    ///
    /// # Errors
    /// The store failing; a stored epoch that is not eight bytes.
    pub fn tail_epoch(&self) -> Result<u64, MembershipError> {
        Ok(self.epoch_cond()?.0)
    }

    /// The epoch and the condition a write of the next one must carry.
    fn epoch_cond(&self) -> Result<(u64, Cond), MembershipError> {
        match self.store.get(TAIL_EPOCH)? {
            None => Ok((0, Cond::Absent)),
            Some(stored) => {
                let bytes = <[u8; 8]>::try_from(stored.value.as_slice())
                    .map_err(|_| MembershipError::Corrupt("tail epoch is not a u64".to_owned()))?;
                Ok((u64::from_be_bytes(bytes), Cond::Version(stored.version)))
            }
        }
    }

    /// Moves every joiner's mark one phase along, as far as the rows allow. A mark that does
    /// not decode is warned about and left. Without a [`super::Handoff`], nothing.
    ///
    /// # Errors
    /// The store failing.
    pub(super) fn advance(&self) -> Result<(), MembershipError> {
        if self.handoff.is_none() {
            return Ok(());
        }
        for (key, _) in self.store.scan_prefix(UNFILLED)? {
            let Some(id) = key
                .strip_prefix(UNFILLED)
                .and_then(|id| id.parse::<u64>().ok())
            else {
                continue;
            };
            self.advance_one(MemberId(id))?;
        }
        Ok(())
    }

    /// One joiner's mark, moved along up to [`STEPS`] times, each a compare-and-set on the
    /// mark and a check on the map it was judged by.
    fn advance_one(&self, id: MemberId) -> Result<(), MembershipError> {
        for _ in 0..STEPS {
            let (Some(mark), Some(stored)) =
                (self.store.get(&unfilled_key(id))?, self.store.get(MAP_KEY)?)
            else {
                return Ok(());
            };
            let map = decode_map(&stored)?;
            if map.member(id).map(|m| m.state) != Some(MemberState::Joining) {
                return Ok(());
            }
            let Ok(before) = Unfilled::decode(mark.value.as_slice()) else {
                tracing::warn!(member = id.0, "an unfilled mark that does not decode; left");
                return Ok(());
            };
            let (epoch, epoch_cond) = self.epoch_cond()?;
            let rows = self.read_rows()?;
            let after = self.fence_step(id, &before, &map, &rows, self.store.now(), epoch);
            if after == before {
                return Ok(());
            }
            let mut txn = Txn::new()
                .put(
                    unfilled_key(id),
                    after.encode_to_vec(),
                    Cond::Version(mark.version),
                )
                .check(MAP_KEY, Cond::Version(stored.version));
            if after.epoch != before.epoch {
                txn = txn.put(TAIL_EPOCH, after.epoch.to_be_bytes().to_vec(), epoch_cond);
            }
            if self.store.txn(txn)?.is_ok() && after.epoch != before.epoch {
                tracing::info!(
                    member = id.0,
                    epoch = after.epoch,
                    generation = map.generation(),
                    "tail fence opened for a joiner"
                );
            }
        }
        Ok(())
    }

    /// The mark of joiner `id` after the step the map and rows allow (`before` itself when
    /// none), `epoch` being the farm's current tail epoch. Pure.
    fn fence_step(
        &self,
        id: MemberId,
        before: &Unfilled,
        map: &Map,
        rows: &BTreeMap<MemberId, Live>,
        now: StoreTime,
        epoch: u64,
    ) -> Unfilled {
        let mut next = before.clone();
        let downs = map.members().iter().any(|m| m.state == MemberState::Down);
        next.degraded |= downs;
        let (Some(fp), boots) = (fill_fp(map, id), write_owner_boots(map)) else {
            return next;
        };
        if before.phase == FLUSHING {
            if before.generation == 0 {
                next.generation = map.generation();
            } else if !downs
                && self
                    .flush_blocks(map, rows, now, before.generation)
                    .is_empty()
            {
                next.phase = ADOPTING;
                next.epoch = epoch + 1;
                next.fp = fp;
                next.fence_boots = boots;
                next.settled_nanos = 0;
            }
        } else if before.fp != fp || before.fence_boots != boots {
            // What was reported under the old fence proves nothing now.
            next.degraded |= before
                .fence_boots
                .iter()
                .any(|f| map.member(MemberId(f.member)).is_none());
            next.epoch = epoch + 1;
            next.fp = fp;
            next.fence_boots = boots;
            next.settled_nanos = 0;
        }
        next
    }

    /// What keeps the flush barrier of generation `need` from holding: a `Down` member, and
    /// each other member whose report is not fresh or does not say `flush_clear_gen >= need`.
    pub(super) fn flush_blocks(
        &self,
        map: &Map,
        rows: &BTreeMap<MemberId, Live>,
        now: StoreTime,
        need: u64,
    ) -> Vec<Block> {
        let freshness = self.config.freshness();
        let mut blocks = Vec::new();
        for member in map.members() {
            if member.state == MemberState::Down {
                blocks.push(Block::Down(member.id));
                continue;
            }
            let have = rows.get(&member.id).and_then(|live| {
                live.report
                    .filter(|report| now.since(report.at) <= freshness)
                    .and(live.fill.flush_clear_gen)
            });
            if have.is_none_or(|have| have < need) {
                blocks.push(Block::Flushing {
                    member: member.id,
                    have,
                    need,
                });
            }
        }
        blocks
    }
}
