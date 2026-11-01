//! What the pack layer counts, rendered with the rest of `GET /metrics`.

use std::sync::atomic::{AtomicU64, Ordering};

use super::RejectReason;
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
    /// Digests asked of the index by FindMissingBlobs and by the checks of an action's inputs and
    /// of an action result's blobs.
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
    /// Packs a daemon stored that were recorded in the index by `CommitPack`.
    pub commits: Counter,
    /// Commits of a pack that were refused, and nothing was recorded: the pack is not in the
    /// store, a sampled blob is not its digest, the name or the list is not one a daemon may
    /// commit, or the pack was recorded before with other contents.
    pub commit_rejects: Counter,
    /// Packs a build wrote through a presigned write that were recorded by
    /// `commit_untrusted_pack`.
    pub(super) untrusted_commits: Counter,
    /// Commits of an untrusted pack that were refused, by [`UNTRUSTED_REASONS`].
    pub(super) untrusted_rejects: [Counter; UNTRUSTED_REASONS.len()],
    /// Packs a task wrote that `ingest` turned into a result.
    pub(super) ingests: Counter,
    /// Packs `ingest` refused, by [`RejectReason`] in the order of its variants.
    pub(super) ingest_rejects: [Counter; RejectReason::ALL.len()],
    /// Ingests that the store, the index or the action cache could not answer.
    pub(super) ingest_unavailable: Counter,
    /// The packs of the runs `ingest` turned into a result (one for a run of one pack).
    pub(super) ingest_packs: Counter,
    /// The bytes of large objects `ingest` stored in the CAS for the runs it turned into a result.
    pub(super) ingest_large_bytes: Counter,
    /// Large objects `ingest` also put to `cas/l/<sha256>`, by [`LargeWriteThrough`] in the order of its
    /// variants.
    pub(super) ingest_large_writethrough: [Counter; LargeWriteThrough::ALL.len()],
}

/// How the write-through of one large object to its content-bound key ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LargeWriteThrough {
    /// The object was written.
    Ok,
    /// The store already held the key (a create-only refusal): the key is bound to the bytes, so
    /// whoever wrote it wrote these.
    Exists,
    /// The store failed the write, or held an object of another length under the key; nothing
    /// was kept under the key by this write.
    Failed,
    /// The ingest gave the write up before its end (the CAS write stopped, the bytes were not the
    /// blob, a retry started the pass over, or the store did not take a window in time): nothing
    /// was kept under the key. Not a failure of the store.
    Aborted,
}

impl LargeWriteThrough {
    /// Every ending, in the order of the variants.
    pub const ALL: [Self; 4] = [Self::Ok, Self::Exists, Self::Failed, Self::Aborted];

    /// The `result` label of the ending.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Exists => "exists",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// Why a commit of an untrusted pack was refused: the key is not the one issued, the manifest
/// cannot be a pack's, the footer is not the manifest, a record is not its digest, the pack is not
/// in the store as said, or the key was recorded with other contents.
#[derive(Clone, Copy, Debug)]
pub(super) enum Reason {
    Forbidden,
    Shape,
    Footer,
    Mismatch,
    Missing,
    Conflict,
}

/// The label of each [`Reason`], in the order of the variants.
const UNTRUSTED_REASONS: [&str; 6] = [
    "forbidden",
    "shape",
    "footer",
    "mismatch",
    "missing",
    "conflict",
];

impl PackMetrics {
    /// The packs `ingest` turned into a result: the action ran, whether or not it was cached.
    #[must_use]
    pub fn ingests(&self) -> u64 {
        self.ingests.get()
    }

    /// The packs `ingest` refused for `reason`.
    #[must_use]
    pub fn ingest_rejects(&self, reason: RejectReason) -> u64 {
        self.ingest_rejects[reason as usize].get()
    }

    /// The ingests that the store, the index or the action cache could not answer.
    #[must_use]
    pub fn ingest_unavailable(&self) -> u64 {
        self.ingest_unavailable.get()
    }

    /// The packs of the runs `ingest` turned into a result: with [`PackMetrics::ingests`], the
    /// packs per run.
    #[must_use]
    pub fn ingest_packs(&self) -> u64 {
        self.ingest_packs.get()
    }

    /// The bytes of large objects `ingest` stored in the CAS, for the runs it turned into a
    /// result.
    #[must_use]
    pub fn ingest_large_bytes(&self) -> u64 {
        self.ingest_large_bytes.get()
    }

    /// The large objects of ingested runs whose write-through to `cas/l/<sha256>` ended as `how`.
    #[must_use]
    pub fn ingest_large_writethrough(&self, how: LargeWriteThrough) -> u64 {
        self.ingest_large_writethrough[how as usize].get()
    }

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
                "Digests asked of the pack index by FindMissingBlobs and by action checks",
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
            (
                "kbf_pack_commits_total",
                "Packs a daemon stored that CommitPack recorded in the pack index",
                &self.commits,
            ),
            (
                "kbf_pack_commit_rejects_total",
                "Commits of a pack refused, nothing recorded: the pack was not in the store, a \
                 sampled blob was not its digest, or the commit was not one a daemon may make",
                &self.commit_rejects,
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
        families.counter(
            "kbf_pack_untrusted_commits_total",
            "Packs a build wrote through a presigned write that were recorded in the pack index",
            self.untrusted_commits.get(),
        );
        families.head(
            "kbf_pack_untrusted_commit_rejects_total",
            "counter",
            "Commits of such a pack refused, nothing recorded, by why",
        );
        for (reason, counter) in UNTRUSTED_REASONS.iter().zip(&self.untrusted_rejects) {
            let line = format!(
                "kbf_pack_untrusted_commit_rejects_total{{reason=\"{reason}\"}} {}\n",
                counter.get()
            );
            families.0.push_str(&line);
        }
        families.counter(
            "kbf_pack_ingests_total",
            "Packs a task wrote that were ingested into an action result: the action ran, \
             whether or not it was cached",
            self.ingests.get(),
        );
        families.head(
            "kbf_pack_ingest_rejects_total",
            "counter",
            "Packs a task wrote that ingest refused, by why",
        );
        for (reason, counter) in RejectReason::ALL.iter().zip(&self.ingest_rejects) {
            let line = format!(
                "kbf_pack_ingest_rejects_total{{reason=\"{}\"}} {}\n",
                reason.label(),
                counter.get()
            );
            families.0.push_str(&line);
        }
        families.counter(
            "kbf_pack_ingest_unavailable_total",
            "Ingests of a task's pack that the object store, the pack index or the action cache \
             could not answer",
            self.ingest_unavailable.get(),
        );
        families.counter(
            "kbf_pack_ingest_packs_total",
            "Packs of the runs a task wrote that were ingested into an action result: divided \
             by the ingests, the packs per run",
            self.ingest_packs.get(),
        );
        families.counter(
            "kbf_pack_ingest_large_bytes_total",
            "Bytes of large objects a task wrote that ingest stored in the CAS, for the runs \
             it ingested into an action result",
            self.ingest_large_bytes.get(),
        );
        families.head(
            "kbf_pack_ingest_large_writethrough_total",
            "counter",
            "Large objects a task wrote that ingest also put to cas/l/<sha256> in the object \
             store, by how the put ended (ok, exists, failed, aborted); best effort, never fails an ingest",
        );
        for (how, counter) in LargeWriteThrough::ALL
            .iter()
            .zip(&self.ingest_large_writethrough)
        {
            let line = format!(
                "kbf_pack_ingest_large_writethrough_total{{result=\"{}\"}} {}\n",
                how.label(),
                counter.get()
            );
            families.0.push_str(&line);
        }
        *out = families.0;
    }
}
