//! The local write tier's invariants (docs/design/write-tier-scenarios.md, section 1),
//! as pure checks the [`Checker`](super::Checker) runs after every step:
//!
//! - **A and B**, the tier half: [`TierHomes`] answers properties 8 and 9 from the
//!   store and from a staged object's block as a power cut would leave it; a block a
//!   start found missing or short, and a read that found a present blob's bytes
//!   nowhere, break property 8 ([`tier_seen`]). Acknowledgements in an object a bad
//!   sector damaged are held to the disk-loss clause instead ([`damaged_acks`]).
//! - **C**, property 10 ([`ids_never_move`]): a blob's location never changes unless
//!   its object was marked unreachable, and no store object is replaced by other
//!   bytes.
//! - **D**, property 11 ([`within_budget`]): the tier's counted and flushed bytes stay
//!   within its budget.
//! - **E**, property 12 ([`directory_leaks`], run at each start): every file in the
//!   tier directory is `TIER`, a staged object's block at its committed length, or a
//!   flushed copy of an object no longer staged; the budget counts the staged bytes.
//! - **F**, property 13 ([`drained`], at the end): no object is left staged.

use std::collections::{BTreeMap, BTreeSet};

use kbf_meta::{BlobAnswer, Location, MetaState, ObjectId, StagedState};
use kbf_store::FaultFs;
use kbf_tier::{TIER, Usage, block_name, parse_block_name, parse_seg_name};
use kbf_types::{Digest, FarmTime};

use super::{Property, hold_if};
use crate::content;
use crate::replica::Broken;
use crate::server::TierSeen;
use crate::store::{BlobHomes, ObjectStore};

/// Where a blob's bytes are durable with the tier: in the store, or in a staged
/// object's block (as `disk`, the durable view, holds it) at the offset the
/// committed staged list gives it.
pub(crate) struct TierHomes<'a> {
    pub(crate) store: &'a ObjectStore,
    pub(crate) disk: &'a FaultFs,
    pub(crate) meta: &'a MetaState,
}

impl BlobHomes for TierHomes<'_> {
    fn holds(&self, object: ObjectId, digest: &Digest) -> bool {
        if self.store.holds(object, digest) {
            return true;
        }
        let Some(staged) = self.meta.staged(object) else {
            return false;
        };
        let Some((_, at)) = staged.blobs().find(|(d, _)| *d == digest) else {
            return false;
        };
        let block = self.disk.contents(&block_name(object)).unwrap_or_default();
        let from = usize::try_from(at).unwrap_or(usize::MAX);
        let to = from.saturating_add(usize::try_from(digest.size_bytes).unwrap_or(0));
        block.get(from..to) == Some(content(digest).as_slice())
    }
}

/// What the tier recorded that breaks an invariant on its own: a lost block or an
/// unreadable present blob (A), a directory that disagrees with the committed state at
/// a start (E), a `PutStaged` the state machine refused.
pub(crate) fn tier_seen(seen: &TierSeen) -> Result<(), Broken> {
    let lost = seen
        .lost
        .first()
        .map(|o| format!("staged object {o} lost at a start"));
    hold_if(lost, Property::NoFalseAck)?;
    hold_if(seen.unread.first().cloned(), Property::NoFalseAck)?;
    let refused = seen
        .refused
        .first()
        .map(|r| format!("PutStaged refused: {r}"));
    hold_if(refused, Property::NothingHalfIndexed)?;
    hold_if(seen.leaks.first().cloned(), Property::NoLeak)
}

/// Property 11: the tier's counted and flushed bytes within its budget.
pub(crate) fn within_budget(usage: Usage) -> Result<(), Broken> {
    let held = usage.used.saturating_add(usage.flushed);
    let detail = format!(
        "{} counted and {} flushed bytes pass a budget of {}",
        usage.used, usage.flushed, usage.budget
    );
    hold_if(
        (held > usage.budget).then_some(detail),
        Property::WithinBudget,
    )
}

/// Invariant E at a start: each of `names` (with its length) against the committed
/// `staged` objects of this tier (with their committed lengths), and the tier's
/// counted bytes `used`.
pub(crate) fn directory_leaks(
    names: &[(String, u64)],
    staged: &BTreeMap<ObjectId, u64>,
    used: u64,
) -> Vec<String> {
    let mut leaks = Vec::new();
    for (name, len) in names {
        if name == TIER {
            continue;
        }
        if let Some(object) = parse_block_name(name) {
            match staged.get(&object) {
                None => leaks.push(format!("{name}: a block no staged object names")),
                Some(committed) if len != committed => {
                    leaks.push(format!("{name}: {len} bytes, {committed} committed"));
                }
                Some(_) => {}
            }
        } else if let Some(object) = parse_seg_name(name) {
            if staged.contains_key(&object) {
                leaks.push(format!("{name}: a flushed copy of a staged object"));
            }
        } else {
            leaks.push(format!("{name}: not a file the tier writes"));
        }
    }
    let total: u64 = staged.values().sum();
    if used != total {
        leaks.push(format!(
            "the budget counts {used} bytes, {total} are staged"
        ));
    }
    leaks
}

/// Property 10 over this step's `answers`: each blob's first location is kept in
/// `first`; a later one must equal it unless its object is `unreachable` (a heal by
/// re-upload). And no store object was `rewritten` with other content.
pub(crate) fn ids_never_move(
    first: &mut BTreeMap<Digest, Location>,
    answers: &[(&Digest, BlobAnswer)],
    unreachable: &dyn Fn(ObjectId) -> bool,
    rewritten: &BTreeSet<ObjectId>,
) -> Result<(), Broken> {
    let replaced = rewritten
        .first()
        .map(|o| format!("{o} replaced in the store by other bytes"));
    hold_if(replaced, Property::IdsNeverMove)?;
    for (digest, answer) in answers {
        let BlobAnswer::Present { location, .. } = answer else {
            continue;
        };
        let was = *first.entry(**digest).or_insert(*location);
        if was != *location && !unreachable(was.object) {
            let d = format!(
                "{digest:?} moved from {} to {}",
                was.object, location.object
            );
            return Err((Property::IdsNeverMove, d));
        }
        first.insert(**digest, *location);
    }
    Ok(())
}

/// Invariant A's disk-loss clause for each acknowledgement in a `damaged` object: the
/// blob is never answered `Absent` (only `Unavailable`, or present after a re-upload).
pub(crate) fn damaged_acks(
    acked: &[(FarmTime, Digest, ObjectId)],
    damaged: &BTreeSet<ObjectId>,
    answer: &dyn Fn(&Digest) -> BlobAnswer,
) -> Result<(), Broken> {
    let absent = acked
        .iter()
        .filter(|(_, _, o)| damaged.contains(o))
        .find(|(_, d, _)| matches!(answer(d), BlobAnswer::Absent));
    let detail = absent.map(|(_, d, o)| format!("{d:?} acknowledged in damaged {o}, now absent"));
    hold_if(detail, Property::NoFalseAck)
}

/// Property 13 at the end: no object left staged and unflushed.
pub(crate) fn drained(meta: &MetaState) -> Result<(), Broken> {
    let left = meta
        .staged_objects()
        .find(|(_, s)| s.state() != StagedState::Conflict);
    let detail = left.map(|(o, s)| format!("{o} still staged ({:?}) at the end", s.state()));
    hold_if(detail, Property::Drained)
}

#[cfg(test)]
mod tests {
    use kbf_meta::{Command, Epoch, ServerId, StagedPut, StoreId, TierId};
    use kbf_types::DigestFunction;

    use super::*;
    use crate::Msg;

    fn object(seq: u64) -> ObjectId {
        ObjectId::new(Epoch::new(1), seq)
    }

    fn digest(n: u8) -> Digest {
        Digest::new(DigestFunction::Sha256, [n; 32], 3)
    }

    fn at(seq: u64) -> Location {
        Location {
            store: StoreId::new(0),
            object: object(seq),
            offset: 0,
        }
    }

    fn present(seq: u64) -> BlobAnswer {
        BlobAnswer::Present {
            location: at(seq),
            staged_on: None,
        }
    }

    /// A state with `digest(1)` staged at offset 0 of object 1 on tier 1.
    fn staged_state() -> MetaState {
        let mut m = MetaState::new(kbf_meta::Retention::default());
        m.execute(1, Command::AllocEpoch);
        let tier = TierId::new(1);
        let add = Command::AddTier {
            server: ServerId::new(1),
            tier,
        };
        m.execute(2, add);
        let put = StagedPut {
            object: object(1),
            end: 3,
            seal: false,
            blobs: vec![(digest(1), 0)],
        };
        m.execute(
            3,
            Command::PutStaged {
                tier,
                objects: vec![put],
            },
        );
        m
    }

    /// Catches homes that take a staged blob as held without reading its block, read
    /// another object's block, or accept other bytes.
    #[test]
    fn a_staged_blob_is_held_only_by_its_block_s_bytes() {
        let meta = staged_state();
        let store = ObjectStore::default();
        let disk = FaultFs::new();
        let homes = TierHomes {
            store: &store,
            disk: &disk,
            meta: &meta,
        };
        assert!(!homes.holds(object(1), &digest(1)), "no block yet");
        disk.put(&block_name(object(1)), &content(&digest(1)));
        assert!(homes.holds(object(1), &digest(1)));
        assert!(!homes.holds(object(2), &digest(1)), "another object");
        assert!(!homes.holds(object(1), &digest(2)), "another blob");
        disk.flip(&block_name(object(1)), 2);
        assert!(!homes.holds(object(1), &digest(1)), "other bytes");
        // Once flushed, the store holds it: object 3 PUT with digest 2.
        let mut store = ObjectStore::default();
        let put = Msg::Put {
            object: object(3),
            digest: digest(2),
        };
        let from = kbf_sim::NodeId::from("server");
        let event = kbf_sim::Event::Message { from, msg: put };
        let input = kbf_sim::NodeInput {
            now: FarmTime::default(),
            entropy: 0,
            event,
        };
        kbf_types::StateMachine::apply(&mut store, input);
        let homes = TierHomes {
            store: &store,
            disk: &disk,
            meta: &meta,
        };
        assert!(homes.holds(object(3), &digest(2)));
    }

    /// Catches a budget check that ignores flushed copies, or fails at the budget
    /// itself.
    #[test]
    fn counted_and_flushed_bytes_past_the_budget_break_it() {
        let usage = |used, flushed| Usage {
            budget: 10,
            used,
            flushed,
            evicted: 0,
        };
        assert!(within_budget(usage(6, 4)).is_ok());
        let broken = |u| within_budget(u).expect_err("over").0;
        assert_eq!(broken(usage(6, 5)), Property::WithinBudget);
        assert_eq!(broken(usage(11, 0)), Property::WithinBudget);
    }

    /// Catches a directory check that misses an orphan block, a block longer than its
    /// committed length, a flushed copy of a staged object, a stray file, or a budget
    /// that counts bytes nothing holds; and one that flags `TIER` or a good block.
    #[test]
    fn each_leak_of_the_tier_directory_is_reported() {
        let staged = BTreeMap::from([(object(1), 5)]);
        let good = [
            (TIER.to_owned(), 8),
            (block_name(object(1)), 5),
            (kbf_tier::seg_name(object(2)), 4),
        ];
        assert_eq!(directory_leaks(&good, &staged, 5), Vec::<String>::new());
        let bad = [
            (block_name(object(1)), 6),
            (block_name(object(3)), 1),
            (kbf_tier::seg_name(object(1)), 5),
            ("stray".to_owned(), 0),
        ];
        let leaks = directory_leaks(&bad, &staged, 7);
        assert_eq!(leaks.len(), 5, "{leaks:?}");
    }

    /// Catches a location check that lets a blob move to another object, or flags a
    /// move after its object was marked unreachable (a heal), and a store rewrite
    /// missed.
    #[test]
    fn a_moved_location_or_a_rewrite_breaks_ids_never_move() {
        let d = digest(1);
        let mut first = BTreeMap::new();
        let none = BTreeSet::new();
        assert!(ids_never_move(&mut first, &[(&d, present(1))], &|_| false, &none).is_ok());
        assert!(ids_never_move(&mut first, &[(&d, BlobAnswer::Absent)], &|_| false, &none).is_ok());
        let moved = ids_never_move(&mut first, &[(&d, present(2))], &|_| false, &none);
        assert_eq!(moved.expect_err("moved").0, Property::IdsNeverMove);
        let healed = ids_never_move(&mut first, &[(&d, present(2))], &|o| o == object(1), &none);
        assert!(healed.is_ok());
        assert_eq!(first[&d], at(2), "the healed location is the one kept");
        let rewritten = BTreeSet::from([object(4)]);
        let r = ids_never_move(&mut first, &[], &|_| false, &rewritten);
        assert_eq!(r.expect_err("rewritten").0, Property::IdsNeverMove);
    }

    /// Catches a damaged acknowledgement allowed to be answered absent, and one in an
    /// undamaged object held to the weaker clause.
    #[test]
    fn a_damaged_ack_may_be_unavailable_never_absent() {
        let d = digest(1);
        let acked = [(FarmTime::from_millis(1), d, object(1))];
        let damaged = BTreeSet::from([object(1)]);
        assert!(damaged_acks(&acked, &damaged, &|_| BlobAnswer::Unavailable).is_ok());
        let absent = damaged_acks(&acked, &damaged, &|_| BlobAnswer::Absent);
        assert_eq!(absent.expect_err("absent").0, Property::NoFalseAck);
        assert!(damaged_acks(&acked, &BTreeSet::new(), &|_| BlobAnswer::Absent).is_ok());
    }

    /// Catches an end check that misses a staged object, and each record of the tier
    /// that must break its invariant.
    #[test]
    fn what_the_tier_recorded_breaks_its_invariant() {
        let meta = staged_state();
        assert_eq!(drained(&meta).expect_err("staged").0, Property::Drained);
        let empty = MetaState::new(kbf_meta::Retention::default());
        assert!(drained(&empty).is_ok());
        assert!(tier_seen(&TierSeen::default()).is_ok());
        type Break = fn(&mut TierSeen);
        let cases: [(Break, Property); 4] = [
            (|s| s.lost.push(object(1)), Property::NoFalseAck),
            (|s| s.unread.push("x".to_owned()), Property::NoFalseAck),
            (
                |s| s.refused.push("x".to_owned()),
                Property::NothingHalfIndexed,
            ),
            (|s| s.leaks.push("x".to_owned()), Property::NoLeak),
        ];
        for (break_it, property) in cases {
            let mut seen = TierSeen::default();
            break_it(&mut seen);
            assert_eq!(tier_seen(&seen).expect_err("broken").0, property);
        }
    }
}
