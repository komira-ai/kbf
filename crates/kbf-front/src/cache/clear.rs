//! What a node says about its unflushed objects: the generation at which everything it
//! was to flush is flushed ([`FlushReport::clear_gen`]), and its backlog. A handoff
//! (`docs/design/rebalance-handoff.md`, P2) waits for every member to report a clear
//! generation at or beyond the one it began at, so that nothing staged before it is
//! still only on a node's disk.
//!
//! - **Dated by the map the node held.** An object is dated, in memory, by the
//!   generation of the map the node's flush gate held when its first `PutStaged`
//!   committed ([`Born::note`]). The date is volatile: an object the process did not
//!   date (staged before a restart, or while the gate held no map) counts as staged under
//!   generation 0, the oldest, until it is flushed. A restart therefore holds the clear
//!   generation back until the objects it found are flushed, never lets it run ahead.
//! - **Only what this node flushes.** The objects counted are the staged ones the gate
//!   says this node flushes under the map it holds now: an object only another node
//!   flushes (the replica's copy of a primary's) stays staged here, off the queue, and
//!   would otherwise hold the generation back for good. Its owner reports it. When a map
//!   change makes an object this node's to flush, it counts from then, at the date it
//!   has, so the generation can go down as well as up: it is what is true now.
//! - **Generation, not time.** The clear generation is the date of the oldest object this
//!   node has yet to flush (an object dated `s` was staged before the node held any map
//!   newer than `s`), or the held generation when there is none. A node without a gate,
//!   or whose gate holds no map, cannot say: `None`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_meta::ObjectId;
use kbf_objstore::ObjectStore;
use kbf_storage::flush::FlushGate;

use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

/// The generation each staged object was staged under, and the gate that says it.
#[derive(Default)]
pub(super) struct Born {
    gate: Mutex<Option<Arc<dyn FlushGate>>>,
    at: Mutex<Dates>,
}

/// The dates, each with the note's sequence number, so a prune can tell a note made
/// after its query began (whose object the query may not have seen) from an older one.
#[derive(Default)]
struct Dates {
    seq: u64,
    by_object: BTreeMap<ObjectId, (u64, u64)>,
}

impl std::fmt::Debug for Born {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Born").finish_non_exhaustive()
    }
}

impl Born {
    /// Dates objects staged from now on by `gate`'s map.
    pub(super) fn judge_by(&self, gate: Arc<dyn FlushGate>) {
        *self.gate.lock().unwrap_or_else(PoisonError::into_inner) = Some(gate);
    }

    /// Records that `object` is staged now: the generation held, else 0 (not known). The
    /// first record of an object stands: a block takes several `PutStaged`s, and is as
    /// old as the first.
    pub(super) fn note(&self, object: ObjectId) {
        let gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        let held = gate.as_ref().and_then(|g| g.generation()).unwrap_or(0);
        drop(gate);
        let mut at = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        at.seq += 1;
        let seq = at.seq;
        at.by_object.entry(object).or_insert((held, seq));
    }

    /// A mark to take before a query of the staged set, for [`Born::prune`].
    pub(super) fn mark(&self) -> u64 {
        self.at.lock().unwrap_or_else(PoisonError::into_inner).seq
    }

    /// Forgets the date of every object not in `staged`, the staged set a query made
    /// after [`Born::mark`] returned `mark` saw. A date noted after the mark stays: its
    /// object may have been staged after the query.
    pub(super) fn prune(&self, mark: u64, staged: &BTreeSet<ObjectId>) {
        let mut at = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        at.by_object
            .retain(|object, (_, seq)| *seq > mark || staged.contains(object));
    }

    /// The clear generation given `held` and the objects this node still has to flush,
    /// forgetting every date of an object not in `staged` ([`Born::prune`]).
    fn clear(&self, held: u64, mark: u64, staged: &BTreeSet<ObjectId>, mine: &[ObjectId]) -> u64 {
        self.prune(mark, staged);
        let at = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        let oldest = mine
            .iter()
            .map(|o| at.by_object.get(o).map_or(0, |(held, _)| *held));
        oldest.fold(held, u64::min)
    }

    /// How many objects have a date.
    pub(super) fn dated(&self) -> usize {
        self.at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .by_object
            .len()
    }
}

/// What a node reports of its unflushed objects ([`Cache::flush_report`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    /// The highest generation `g` such that every object this node was to flush that it
    /// staged before it held map `g` is flushed; `None` when the node cannot say (no
    /// gate, or a gate holding no map).
    pub clear_gen: Option<u64>,
    /// The objects staged on this node's tier and not flushed (open, sealed, or in
    /// conflict), whoever flushes them.
    pub staged_objects: u64,
    /// The committed bytes of those objects.
    pub staged_bytes: u64,
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// How many objects this node holds a staging date for. A date lives while its object
    /// is staged on this node's tier, and is dropped by the next flush round
    /// ([`Cache::flush_tier`]) or report after the object leaves staged state, so this
    /// is bounded by the staged objects and the objects staged since.
    #[must_use]
    pub fn dated_objects(&self) -> usize {
        self.writer.born.dated()
    }

    /// The report of this node's unflushed objects now.
    ///
    /// # Errors
    ///
    /// The metadata log could not answer the query.
    pub async fn flush_report(&self) -> Result<FlushReport, CacheError> {
        // Before the query, so a map taken in between only lowers what is said.
        let held = self.gate.as_ref().and_then(|g| g.generation());
        let mark = self.writer.born.mark();
        let Some(staging) = self.writer.tier.get() else {
            return Ok(FlushReport {
                clear_gen: held,
                ..FlushReport::default()
            });
        };
        let (tier, gate) = (staging.id, self.gate.clone());
        let (staged, mine, bytes) = self
            .writer
            .query(move |s| {
                let ours = s.staged_objects().filter(|(_, o)| o.tier() == tier);
                let mut staged = BTreeSet::new();
                let (mut mine, mut bytes) = (Vec::new(), 0u64);
                for (object, o) in ours {
                    staged.insert(object);
                    bytes += o.committed_len();
                    if gate
                        .as_ref()
                        .is_some_and(|g| o.blobs().any(|(d, _)| g.flushes(d)))
                    {
                        mine.push(object);
                    }
                }
                (staged, mine, bytes)
            })
            .await?;
        Ok(FlushReport {
            clear_gen: held.map(|held| self.writer.born.clear(held, mark, &staged, &mine)),
            staged_objects: staged.len() as u64,
            staged_bytes: bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kbf_meta::Epoch;
    use kbf_types::{Digest, DigestFunction};

    /// A gate that holds the generation it is made with.
    #[derive(Debug)]
    struct At(u64);

    impl FlushGate for At {
        fn flushes(&self, _: &Digest) -> bool {
            true
        }

        fn generation(&self) -> Option<u64> {
            Some(self.0)
        }
    }

    fn object(n: u64) -> ObjectId {
        ObjectId::new(Epoch::new(1), n)
    }

    /// Catches: a prune that forgets the date of an object staged after its query began
    /// (the query's staged set cannot name it, and it would count as staged under
    /// generation 0, holding the clear generation back for nothing), and one that keeps
    /// the date of an object the query found gone. Object 1 is dated before the mark and
    /// not staged: dropped; object 2 is dated before the mark and staged: kept; object 3
    /// is dated after the mark and not in the set: kept.
    #[test]
    fn a_prune_keeps_the_dates_of_staged_objects_and_of_notes_after_its_mark() {
        let born = Born::default();
        born.note(object(1));
        born.note(object(2));
        let mark = born.mark();
        born.note(object(3));
        born.prune(mark, &BTreeSet::from([object(2)]));
        assert_eq!(born.dated(), 2);
        born.prune(born.mark(), &BTreeSet::new());
        assert_eq!(born.dated(), 0);
    }

    /// Catches: a clear generation that is the held one while an older object is
    /// unflushed, one that forgets an object's first date, and one that does not count an
    /// object it never dated as the oldest. Object 1 is dated 7, object 2 is dated 9; a
    /// date noted again does not move; with both to flush the clear generation is 7, with
    /// only 2 it is 9, with an undated object among them 0, with none the held 12; and
    /// the dates of objects not staged are gone afterwards.
    #[test]
    fn the_clear_generation_is_the_oldest_date_of_what_is_left_to_flush() {
        let born = Born::default();
        let digest = Digest::new(DigestFunction::Sha256, [0; 32], 0);
        assert!(At(1).flushes(&digest));
        assert_eq!(format!("{born:?}"), "Born { .. }");
        born.judge_by(Arc::new(At(7)));
        born.note(object(1));
        born.judge_by(Arc::new(At(9)));
        born.note(object(2));
        born.note(object(1));
        let staged = BTreeSet::from([object(1), object(2)]);
        let mark = born.mark();
        assert_eq!(born.clear(12, mark, &staged, &[object(1), object(2)]), 7);
        assert_eq!(born.clear(12, mark, &staged, &[object(2)]), 9);
        assert_eq!(born.clear(12, mark, &staged, &[object(2), object(3)]), 0);
        assert_eq!(born.clear(12, mark, &staged, &[]), 12);
        let left = BTreeSet::from([object(2)]);
        assert_eq!(born.clear(12, mark, &left, &[object(2)]), 9);
        assert_eq!(born.dated(), 1);
    }
}
