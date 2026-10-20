//! The standing tail of the index ([`Cache::tail_pass`]): what keeps a node's index
//! complete for objects that other members write to the store after its one-shot
//! rebuild ([`Cache::rebuild_index`]) has finished (`docs/design/rebalance-handoff.md`,
//! P3).
//!
//! A pass lists `<root>cas/` (every node writer's objects, `cas/<member>/<boot>/<seq>`,
//! and an epoch writer's where `legacy` is the root, `cas/<epoch>/<seq>`), reads the
//! footer of each object it has not handled, and adopts its records by the command the
//! rebuild uses ([`Command::AdoptSegment`]): only digests the index does not hold. An
//! object of this node's own member is never read: its puts index it, and an earlier
//! boot's objects are the rebuild's. A frozen old-layout prefix (`<root><log id>/cas/`) is
//! outside the listing.
//!
//! There is no cursor. Keys sort by member, then boot, then seq (each 16 lowercase hex
//! digits, so key order is numeric order), but uploads finish in no seq order
//! (`puts_in_flight`): a segment numbered below one already listed can appear later, and
//! a cursor past it would never read it. Each pass lists the whole prefix instead and
//! skips what [`TailState`] holds, so a pass reads one footer per new object and lists
//! the rest by key alone. [`TailState`] keeps the handled seqs of each writer as ranges,
//! which a run of consecutive seqs collapses to one.
//!
//! A pass that returns `Ok` handled every object the listing showed. The listing is not a
//! snapshot, so an object put while it ran may be missed: a caller that needs one that
//! started after some point runs a second pass after the first ([`TailReport::quiet`]).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures::TryStreamExt as _;
use kbf_meta::{ObjectId, Writer};
use kbf_objstore::footers::{FooterRead, FooterReads, SkipReason, read_footers_unless};
use kbf_objstore::{ObjectKey, ObjectStore};
use tokio::time::Instant;

use super::rebuild::Adopted;
use super::{Cache, CacheError, RebuildReport};
use crate::meta_log::MetaLog;

/// The objects a tail has handled, so a later pass does not read them again. Volatile: a
/// new one (a restart) reads every object once, which adopts nothing new for a log that
/// holds them.
#[derive(Clone, Debug, Default)]
pub struct TailState {
    /// Handled seqs of each writer, as sorted, disjoint, non-adjacent inclusive ranges.
    seqs: BTreeMap<Writer, Vec<(u64, u64)>>,
    /// Handled keys that name no object of any writer (foreign bytes under `cas/`).
    junk: BTreeSet<ObjectKey>,
}

impl TailState {
    /// Whether `seq` of `writer` was handled.
    fn holds(&self, writer: Writer, seq: u64) -> bool {
        self.seqs.get(&writer).is_some_and(|ranges| {
            let at = ranges.partition_point(|&(_, end)| end < seq);
            ranges.get(at).is_some_and(|&(start, _)| start <= seq)
        })
    }

    /// Records `seq` of `writer` as handled, joining the ranges next to it. `seq` is one
    /// [`TailState::holds`] answered no to: a pass skips what it holds, so it never
    /// handles a seq twice.
    fn insert(&mut self, writer: Writer, seq: u64) {
        let ranges = self.seqs.entry(writer).or_default();
        let at = ranges.partition_point(|&(_, end)| end < seq);
        let joins_next = ranges
            .get(at)
            .is_some_and(|&(start, _)| seq.checked_add(1) == Some(start));
        let joins_prev = at
            .checked_sub(1)
            .is_some_and(|p| ranges[p].1.checked_add(1) == Some(seq));
        match (joins_prev, joins_next) {
            (true, true) => {
                ranges[at - 1].1 = ranges[at].1;
                ranges.remove(at);
            }
            (true, false) => ranges[at - 1].1 = seq,
            (false, true) => ranges[at].0 = seq,
            (false, false) => ranges.insert(at, (seq, seq)),
        }
    }

    /// How many ranges of seqs are held, over every writer: what the state costs.
    #[must_use]
    pub fn ranges(&self) -> usize {
        self.seqs.values().map(Vec::len).sum()
    }
}

/// What a [`Cache::tail_pass`] found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailReport {
    /// Objects listed that the pass had not handled before, and read.
    pub objects: u64,
    /// Segments whose records were adopted.
    pub segments: u64,
    /// Records newly indexed.
    pub inserted: u64,
    /// Records left alone: their digest was indexed already.
    pub kept: u64,
    /// Objects not read as a segment (too short, torn, or foreign bytes), logged.
    pub skipped: u64,
    /// Objects whose key is not one of this cache's layouts, or of an epoch this log
    /// never allocated; logged.
    pub foreign: u64,
    /// Objects deleted between the listing and the read.
    pub gone: u64,
    /// How long the pass took.
    pub duration: Duration,
}

impl TailReport {
    /// The pass found nothing it had not handled: a pass after a first one that adopted
    /// no object, which is what a listing that lagged a put would otherwise hide.
    #[must_use]
    pub fn quiet(&self) -> bool {
        self.objects == 0
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Adopts the objects under `<root>cas/` that `state` has not handled and that are not
    /// this cache's own member's, and records them in `state`.
    ///
    /// # Errors
    /// The metadata log or the listing fails, or a read of an object's footer fails (not
    /// the object's absence or a damaged footer: those are skipped). What was handled
    /// before the failure stays in `state`, so a later pass does not read it again; the
    /// object that failed is read again.
    pub async fn tail_pass(
        &self,
        state: &mut TailState,
        reads: FooterReads,
    ) -> Result<TailReport, CacheError> {
        let started = Instant::now();
        let prefix = self.writer.keys.root.child("cas/")?;
        let me = match self.writer.names_as {
            Writer::Node(n) => Some(n.member()),
            Writer::Epoch(_) | Writer::Legacy(_) => None,
        };
        let mut sink = RebuildReport::default();
        let mut report = TailReport::default();
        let mut handled: Vec<Handled> = Vec::new();
        let outcome = {
            let held = &*state;
            let keys = &self.writer.keys;
            let skip = move |key: &ObjectKey| match keys.object(key) {
                None => held.junk.contains(key),
                Some(id) => is_member(id, me) || held.holds(id.writer(), id.seq()),
            };
            let mut footers = std::pin::pin!(read_footers_unless(
                &self.writer.objects,
                &prefix,
                None,
                reads,
                skip,
            ));
            self.tail_reads(&mut footers, &mut sink, &mut report, &mut handled)
                .await
        };
        for done in handled {
            match done {
                Handled::Object(id) => state.insert(id.writer(), id.seq()),
                Handled::Junk(key) => {
                    state.junk.insert(key);
                }
            }
        }
        outcome?;
        report.inserted = sink.inserted;
        report.kept = sink.kept;
        report.segments = sink.segments;
        report.duration = started.elapsed();
        Ok(report)
    }

    /// Handles each footer `footers` yields, noting in `handled` what a later pass need
    /// not read again.
    async fn tail_reads<S>(
        &self,
        footers: &mut S,
        sink: &mut RebuildReport,
        report: &mut TailReport,
        handled: &mut Vec<Handled>,
    ) -> Result<(), CacheError>
    where
        S: futures::Stream<Item = Result<FooterRead, kbf_objstore::ObjectStoreError>> + Unpin,
    {
        while let Some(read) = footers.try_next().await? {
            report.objects += 1;
            let key = match read {
                FooterRead::Segment(segment) => {
                    // An object of this cache's current writer is refused by the log, and
                    // none is listed: its member's keys are skipped before the read.
                    if self.adopt(&segment, sink).await? == Adopted::Foreign {
                        report.foreign += 1;
                    }
                    segment.key
                }
                FooterRead::Skipped(skipped) => {
                    match skipped.why {
                        // Not a verdict on the object: stop, and read it again next time.
                        SkipReason::Read(error) => return Err(error.into()),
                        // Gone: not listed again, so nothing to remember.
                        SkipReason::Gone => {
                            report.gone += 1;
                            continue;
                        }
                        why @ (SkipReason::Short | SkipReason::NotASegment(_)) => {
                            tracing::warn!(key = %skipped.key, %why, "tail: not a segment, skipped");
                            report.skipped += 1;
                        }
                    }
                    skipped.key
                }
            };
            handled.push(match self.writer.keys.object(&key) {
                Some(id) => Handled::Object(id),
                None => Handled::Junk(key),
            });
        }
        Ok(())
    }
}

/// What a pass handled, to remember.
enum Handled {
    Object(ObjectId),
    Junk(ObjectKey),
}

/// Whether `id` is an object of node member `me`, any boot.
fn is_member(id: ObjectId, me: Option<std::num::NonZeroU64>) -> bool {
    matches!((id.writer(), me), (Writer::Node(n), Some(me)) if n.member() == me)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    fn writer(member: u64) -> Writer {
        let n = |v| NonZeroU64::new(v).expect("not zero");
        Writer::Node(kbf_meta::NodeWriter::new(n(member), n(1)))
    }

    /// Catches: a range set that forgets a seq, claims one it never saw, or leaves a run of
    /// consecutive seqs as many ranges (the state would grow with every object). Seqs go
    /// in the order that exercises a new range and each join (before, after, both); the
    /// ranges and the answers are checked after each.
    #[test]
    fn seqs_collapse_to_ranges_and_only_those_are_held() {
        let w = writer(5);
        let mut state = TailState::default();
        assert!(!state.holds(w, 3));
        for (seq, ranges) in [(3, 1), (5, 2), (4, 1), (10, 2), (9, 2), (7, 3), (8, 2)] {
            state.insert(w, seq);
            assert!(state.holds(w, seq), "{seq}");
            assert_eq!(state.ranges(), ranges, "after {seq}");
        }
        // A seq beside a range on its low side only, then on its high side only.
        for (seq, ranges) in [(12, 3), (13, 3)] {
            state.insert(w, seq);
            assert_eq!(state.ranges(), ranges, "after {seq}");
        }
        assert_eq!(state.seqs[&w], [(3, 5), (7, 10), (12, 13)]);
        for missing in [0, 2, 6, 11, 14, u64::MAX] {
            assert!(!state.holds(w, missing), "{missing}");
        }
        assert!(!state.holds(writer(6), 3), "another writer's seq 3");
    }

    /// Catches: arithmetic that wraps at the ends of the seq space (a seq of `u64::MAX`
    /// joined to a range that starts at 0, or 0 joined to one that ends at the maximum).
    #[test]
    fn the_ends_of_the_seq_space_join_nothing() {
        let w = writer(5);
        let mut state = TailState::default();
        state.insert(w, u64::MAX);
        state.insert(w, 0);
        assert_eq!(state.seqs[&w], [(0, 0), (u64::MAX, u64::MAX)]);
        assert!(state.holds(w, u64::MAX));
        assert!(state.holds(w, 0));
    }
}
