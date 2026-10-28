//! The write tier's counters and their families, rendered by `Cache::render_metrics`
//! when a tier is attached.

use std::fmt::Write as _;

use super::{Counter, Families, Level};

/// What a cache with a local write tier counts about it (`docs/design/write-path.md`,
/// section 6). Rendered by `Cache::render_metrics` with the tier's usage and exposure,
/// only when a tier is attached.
#[derive(Debug, Default)]
pub struct TierCounters {
    /// Segments staged: synced to a block and committed with `PutStaged`.
    pub staged: Counter,
    /// Segments sent to the store because the tier was at its budget.
    pub full: Counter,
    /// Segments sent to the store because the tier's disk failed before the commit.
    pub errors: Counter,
    /// Objects flushed: PUT, footer read back, `ObjectFlushed` committed.
    pub flushes: Counter,
    /// Flush attempts that failed and will be retried.
    pub flush_failures: Counter,
    /// Objects whose key held another object and were moved to an object of their own (alert).
    pub conflicts: Counter,
    /// Objects with a record that failed its digest on the tier: marked corrupt.
    pub corrupt: Counter,
    /// Records the flusher found not to hash to their digests, and dropped instead of
    /// uploading (alert): the daemon's digests are trusted when an upload is
    /// acknowledged, and checked here.
    pub digest_mismatch: Counter,
    /// Blobs read from the tier's disk (a block or a flushed copy).
    pub local_reads: Counter,
    /// Objects staged on the tier and not flushed, as a flusher round found them at its start.
    pub staged_objects: Level,
    /// The committed bytes of those objects.
    pub staged_bytes: Level,
}

impl TierCounters {
    /// Counts a segment the tier did not take, by why.
    pub(crate) fn direct(&self, error: &kbf_tier::TierError) {
        match error {
            kbf_tier::TierError::Full { .. } => self.full.add(1),
            _ => self.errors.add(1),
        }
    }

    /// Appends the tier's families to `out`: these counters, then `usage` and
    /// `unflushed` as gauges.
    pub fn render(&self, out: &mut String, usage: kbf_tier::Usage, unflushed: kbf_tier::Unflushed) {
        let mut f = Families(std::mem::take(out));
        let counters = [
            (
                "kbf_write_tier_staged_total",
                "Segments staged on the write tier: synced, then committed",
                self.staged.get(),
            ),
            (
                "kbf_write_tier_full_total",
                "Segments written to the store because the write tier was at its budget",
                self.full.get(),
            ),
            (
                "kbf_write_tier_errors_total",
                "Segments written to the store because the write tier's disk failed",
                self.errors.get(),
            ),
            (
                "kbf_write_tier_flushes_total",
                "Staged objects flushed to the store",
                self.flushes.get(),
            ),
            (
                "kbf_write_tier_flush_failures_total",
                "Flush attempts that failed and are retried after a wait",
                self.flush_failures.get(),
            ),
            (
                "kbf_write_tier_flush_conflicts_total",
                "Staged objects whose key in the store held another object (alert)",
                self.conflicts.get(),
            ),
            (
                "kbf_write_tier_corrupt_total",
                "Staged objects with a record that failed its digest on the tier's disk",
                self.corrupt.get(),
            ),
            (
                "kbf_tier_flush_digest_mismatch_total",
                "Records the flusher dropped because they did not hash to their digest (alert)",
                self.digest_mismatch.get(),
            ),
            (
                "kbf_write_tier_local_reads_total",
                "Blobs read from the write tier's disk",
                self.local_reads.get(),
            ),
            (
                "kbf_write_tier_evicted_total",
                "Flushed copies evicted since the start",
                usage.evicted,
            ),
        ];
        for (name, help, value) in counters {
            f.counter(name, help, value);
        }
        let gauges = [
            (
                "kbf_write_tier_budget_bytes",
                "The write tier's budget",
                usage.budget,
            ),
            (
                "kbf_write_tier_used_bytes",
                "Bytes counted against the budget: blocks not yet flushed, open uploads' reservations included",
                usage.used,
            ),
            (
                "kbf_write_tier_flushed_copy_bytes",
                "Bytes of flushed copies kept on the tier for local reads",
                usage.flushed,
            ),
            (
                "kbf_write_tier_staged_objects",
                "Objects staged on the write tier and not flushed, at the start of the last flusher round",
                self.staged_objects.get(),
            ),
            (
                "kbf_write_tier_staged_bytes",
                "Committed bytes of the staged objects not flushed, at the start of the last flusher round",
                self.staged_bytes.get(),
            ),
            (
                "kbf_write_tier_unflushed_bytes",
                "Acknowledged bytes on the write tier not yet in the object store",
                unflushed.bytes,
            ),
            (
                "kbf_write_tier_unflushed_blocks",
                "Blocks holding acknowledged bytes not yet in the object store",
                unflushed.blocks,
            ),
        ];
        for (name, help, value) in gauges {
            f.gauge(name, help, value);
        }
        let oldest = unflushed.oldest.unwrap_or_default().as_secs_f64();
        let name = "kbf_write_tier_oldest_unflushed_seconds";
        f.head(
            name,
            "gauge",
            "Age of the oldest block holding acknowledged bytes not yet in the object store",
        );
        let _ = writeln!(f.0, "{name} {oldest}");
        *out = f.0;
    }
}
