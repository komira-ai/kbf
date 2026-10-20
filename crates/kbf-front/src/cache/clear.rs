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
    at: Mutex<BTreeMap<ObjectId, u64>>,
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
        at.entry(object).or_insert(held);
    }

    /// The clear generation given `held` and the objects this node still has to flush,
    /// forgetting every date of an object not in `staged`.
    fn clear(&self, held: u64, staged: &BTreeSet<ObjectId>, mine: &[ObjectId]) -> u64 {
        let mut at = self.at.lock().unwrap_or_else(PoisonError::into_inner);
        at.retain(|object, _| staged.contains(object));
        let oldest = mine.iter().map(|o| at.get(o).copied().unwrap_or(0));
        oldest.fold(held, u64::min)
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
    /// The report of this node's unflushed objects now.
    ///
    /// # Errors
    ///
    /// The metadata log could not answer the query.
    pub async fn flush_report(&self) -> Result<FlushReport, CacheError> {
        // Before the query, so a map taken in between only lowers what is said.
        let held = self.gate.as_ref().and_then(|g| g.generation());
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
            clear_gen: held.map(|held| self.writer.born.clear(held, &staged, &mine)),
            staged_objects: staged.len() as u64,
            staged_bytes: bytes,
        })
    }
}
