//! What the pack layer counts, rendered with the rest of `GET /metrics`.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::metrics::{Counter, Families};

/// The bytes reserved for uploads now and the most reserved at once.
#[derive(Debug, Default)]
pub(super) struct Reserved {
    now: AtomicU64,
    peak: AtomicU64,
}

impl Reserved {
    /// Counts `bytes` more reserved.
    pub(super) fn take(&self, bytes: u64) {
        let now = self.now.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// Counts `bytes` given back.
    pub(super) fn give(&self, bytes: u64) {
        self.now.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Counters of the pack layer. A family is `kbf_pack_...`; each one's `# HELP` line says what
/// it counts.
#[derive(Debug, Default)]
pub struct PackMetrics {
    /// The bytes that uploads into packs have reserved of `--pack-inflight-bytes`.
    pub(super) reserved: Reserved,
    /// Packs stored and recorded in the index.
    pub writes: Counter,
    /// Bytes of those packs, footers included.
    pub written_bytes: Counter,
    /// Blobs in those packs.
    pub written_blobs: Counter,
    /// Blobs of a write that the index had already, and so were not packed again.
    pub skipped_blobs: Counter,
    /// Writes that failed because the store or the index could not take the packs. A blob that
    /// is not its digest is the client's, and is not counted.
    pub write_failures: Counter,
    /// Digests asked of the index by FindMissingBlobs.
    pub asked: Counter,
    /// Of those, the digests the index had.
    pub found: Counter,
    /// Blobs served from a pack.
    pub reads: Counter,
    /// Bytes of those blobs.
    pub read_bytes: Counter,
    /// Reads of a blob the index does not have, which went to the storage nodes.
    pub fallbacks: Counter,
    /// Reads of a blob the index has and its pack does not, which went to the storage nodes.
    pub lost: Counter,
    /// Reads that failed because the index or the store could not answer.
    pub read_errors: Counter,
    /// Reads of a blob whose pack held other bytes than its digest: never served.
    pub digest_mismatches: Counter,
    /// Uploads that had to wait for room, because the uploads in flight held all of
    /// `--pack-inflight-bytes`.
    pub upload_waits: Counter,
    /// Uploads refused RESOURCE_EXHAUSTED because they alone are over `--pack-inflight-bytes`.
    pub upload_refusals: Counter,
    /// Uploads ended DEADLINE_EXCEEDED because no byte of their body arrived for
    /// `--pack-upload-idle-timeout`.
    pub upload_idle_timeouts: Counter,
}

impl PackMetrics {
    /// The bytes reserved for the uploads in flight now.
    #[must_use]
    pub fn inflight_bytes(&self) -> u64 {
        self.reserved.now.load(Ordering::Relaxed)
    }

    /// The most bytes reserved for uploads at once since these packs were made.
    #[must_use]
    pub fn inflight_peak_bytes(&self) -> u64 {
        self.reserved.peak.load(Ordering::Relaxed)
    }

    /// Appends the families to `out` in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let mut families = Families(std::mem::take(out));
        let counters = [
            (
                "kbf_pack_writes_total",
                "Packs stored and recorded in the pack index",
                &self.writes,
            ),
            (
                "kbf_pack_written_bytes_total",
                "Bytes of the packs written, footers included",
                &self.written_bytes,
            ),
            (
                "kbf_pack_written_blobs_total",
                "Blobs in the packs written",
                &self.written_blobs,
            ),
            (
                "kbf_pack_skipped_blobs_total",
                "Blobs of a write the pack index had already, not packed again",
                &self.skipped_blobs,
            ),
            (
                "kbf_pack_write_failures_total",
                "Writes into packs that the store or the pack index could not take",
                &self.write_failures,
            ),
            (
                "kbf_pack_find_missing_asked_total",
                "Digests FindMissingBlobs asked of the pack index",
                &self.asked,
            ),
            (
                "kbf_pack_find_missing_found_total",
                "Of those, the digests the pack index had",
                &self.found,
            ),
            (
                "kbf_pack_reads_total",
                "Blobs served from a pack",
                &self.reads,
            ),
            (
                "kbf_pack_read_bytes_total",
                "Bytes of the blobs served from a pack",
                &self.read_bytes,
            ),
            (
                "kbf_pack_read_fallbacks_total",
                "Reads of a blob the pack index lacks, answered by the storage nodes",
                &self.fallbacks,
            ),
            (
                "kbf_pack_read_lost_total",
                "Reads of a blob with an index row and no pack, answered by the storage nodes",
                &self.lost,
            ),
            (
                "kbf_pack_read_errors_total",
                "Reads that failed because the pack index or the pack store could not answer",
                &self.read_errors,
            ),
            (
                "kbf_pack_digest_mismatches_total",
                "Reads of a blob whose pack held other bytes than its digest, never served",
                &self.digest_mismatches,
            ),
            (
                "kbf_pack_upload_waits_total",
                "Uploads that waited for room: the uploads in flight held all the bytes allowed",
                &self.upload_waits,
            ),
            (
                "kbf_pack_upload_refusals_total",
                "Uploads refused: alone, they are over the bytes uploads may hold in flight",
                &self.upload_refusals,
            ),
            (
                "kbf_pack_upload_idle_timeouts_total",
                "Uploads ended because no byte of their body arrived within the idle timeout",
                &self.upload_idle_timeouts,
            ),
        ];
        for (name, help, counter) in counters {
            families.counter(name, help, counter.get());
        }
        families.gauge(
            "kbf_pack_inflight_bytes",
            "Bytes reserved for the uploads into packs in flight",
            self.inflight_bytes(),
        );
        families.gauge(
            "kbf_pack_inflight_peak_bytes",
            "The most bytes reserved for uploads into packs at once since the start",
            self.inflight_peak_bytes(),
        );
        *out = families.0;
    }
}
