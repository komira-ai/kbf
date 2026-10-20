//! The joiner's own beat promoting it (`docs/design/rebalance-handoff.md`, section 4,
//! "Phase B"): [`Members::beat_step`], called by the beat of a `Joining` member inside the
//! transaction that rewrites its row. When every condition holds it makes the member
//! `Active` in that same transaction, which also deletes the `unfilled/<id>` mark (checked
//! at the version it was read) and writes `meta/members_formed`, as an operator's
//! `set_state(Active)` does. The row is written at the version it was read, so a state set
//! meanwhile (`Draining`) fails the commit and is never overwritten; the map is checked at
//! the version the conditions were judged by.
//!
//! The conditions, as the beat sees them:
//!
//! 1. the node says its index rebuild is done (this beat's `catching_up` is false);
//! 2. the mark is in the adopting phase ([`super::fence`]: the flush barrier held and an
//!    epoch is open), and the joiner reports `tail_epoch_done` at or above it under the boot
//!    the fence recorded for it;
//! 3. its `fill_fp` equals the fingerprint of the current map with it assumed `Active`, and
//!    the mark's own does (the frontend has not yet opened a newer epoch for a placement
//!    change);
//! 4. no member is `Down` inside its dwell, and no member was during the handoff, unless
//!    [`super::Handoff::allow_degraded`];
//! 5. the settle time has passed since 1 to 4 first held (`settled_nanos` in the mark: the
//!    first beat that finds them holding writes the store's clock there; a beat that finds
//!    one failing clears it).
//!
//! **Not built:** the design's sampled audit (a check, against the joiner's own index, of
//! digests sampled from the objects its tail adopted). It needs `MetaState::sample_digests`
//! (PR 1b of the design) and a place on the wire for its result.

use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_state::{Cond, StoreTime, Txn, Version};
use prost::Message as _;

use super::handoff::{ADOPTING, Block, Fill, Unfilled, fill_fp};
use super::join::unfilled_key;
use super::row::Live;
use super::{MAP_KEY, Members, MembershipError, Progress, decode_map};

/// What a `Joining` member's beat decided about its mark and its promotion.
pub(super) struct Step {
    /// The member becomes `Active` in this transaction.
    pub(super) promote: bool,
    /// The mark's new body, when it changed (settle bookkeeping), and the version it was
    /// read at.
    rewrite: Option<(Vec<u8>, Version)>,
    /// The mark's version and the map's, when the decision rests on them.
    guards: Option<(Version, Version)>,
}

impl Step {
    /// A beat that decides nothing.
    pub(super) const fn none() -> Self {
        Self {
            promote: false,
            rewrite: None,
            guards: None,
        }
    }

    /// `txn` with what this step adds: the mark deleted (a promotion) or rewritten, and the
    /// map checked.
    pub(super) fn apply(&self, mut txn: Txn, id: MemberId) -> Txn {
        let Some((mark, map)) = self.guards else {
            return txn;
        };
        if self.promote {
            txn = txn.delete(unfilled_key(id), Cond::Version(mark));
        } else if let Some((body, version)) = &self.rewrite {
            txn = txn.put(unfilled_key(id), body.clone(), Cond::Version(*version));
        }
        txn.check(MAP_KEY, Cond::Version(map))
    }
}

fn nanos(at: StoreTime) -> u64 {
    u64::try_from(at.as_duration().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

impl Members {
    /// What the beat of `live` (a `Joining` member, its row already carrying what the beat
    /// reported) decides: nothing, a rewrite of the mark's settle time, or the promotion.
    ///
    /// # Errors
    /// The store failing; a stored map that does not decode.
    pub(super) fn beat_step(
        &self,
        live: &Live,
        progress: Progress,
    ) -> Result<Step, MembershipError> {
        let Some(handoff) = self.handoff else {
            return Ok(Step::none());
        };
        let id = live.member.id;
        let (Some(mark), Some(stored)) =
            (self.store.get(&unfilled_key(id))?, self.store.get(MAP_KEY)?)
        else {
            return Ok(Step::none());
        };
        let Ok(body) = Unfilled::decode(mark.value.as_slice()) else {
            return Ok(Step::none());
        };
        if body.phase != ADOPTING {
            return Ok(Step::none());
        }
        let map = decode_map(&stored)?;
        let blocks = self.beat_blocks(&body, live, progress, &map);
        let guards = Some((mark.version, stored.version));
        let now = self.store.now();
        let rewrite = |settled_nanos| {
            let mut next = body.clone();
            next.settled_nanos = settled_nanos;
            Some((next.encode_to_vec(), mark.version))
        };
        if !blocks.is_empty() {
            // One condition fails: the time already counted is lost.
            if body.settled_nanos == 0 {
                return Ok(Step::none());
            }
            return Ok(Step {
                promote: false,
                rewrite: rewrite(0),
                guards,
            });
        }
        if body.settled_nanos == 0 && !handoff.settle.is_zero() {
            return Ok(Step {
                promote: false,
                rewrite: rewrite(nanos(now)),
                guards,
            });
        }
        let since = StoreTime::from_duration(Duration::from_nanos(body.settled_nanos));
        Ok(Step {
            promote: handoff.settle.is_zero() || now.since(since) >= handoff.settle,
            rewrite: None,
            guards,
        })
    }

    /// The conditions of a joiner's own beat that do not hold (not the settle time, which
    /// [`Members::beat_step`] counts), for `body` the mark of `live` and `map` the map now.
    pub(super) fn beat_blocks(
        &self,
        body: &Unfilled,
        live: &Live,
        progress: Progress,
        map: &kbf_map::Map,
    ) -> Vec<Block> {
        let id = live.member.id;
        let fill: Fill = live.fill;
        let mut blocks = Vec::new();
        if progress == Progress::CatchingUp {
            blocks.push(Block::Rebuilding);
        }
        if fill.tail_epoch_done < body.epoch {
            blocks.push(Block::TailPending {
                done: fill.tail_epoch_done,
                need: body.epoch,
            });
        }
        let fenced = body.fenced_boot(id);
        if fenced != Some(live.boot) {
            blocks.push(Block::Boot {
                have: live.boot,
                fenced,
            });
        }
        let now_fp = fill_fp(map, id);
        if now_fp != Some(fill.fill_fp) || now_fp != Some(body.fp) {
            blocks.push(Block::Fingerprint);
        }
        if !self.handoff.is_some_and(|h| h.allow_degraded) {
            blocks.extend(
                map.members()
                    .iter()
                    .filter(|m| m.state == MemberState::Down)
                    .map(|m| Block::Down(m.id)),
            );
            if body.degraded {
                blocks.push(Block::Degraded);
            }
        }
        blocks
    }
}
