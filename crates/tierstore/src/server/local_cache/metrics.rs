//! The counters of the local cache, shown by `GET /metricz` in the Prometheus text format.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a fill did not leave an object in the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailedFill {
    /// A key named by its content whose bytes hash to another name.
    HashMismatch,
    /// The upstream did not give the object (down, refused, an error).
    UpstreamError,
    /// The upstream gave fewer bytes than the size it had said.
    ShortBody,
    /// The cache directory could not be written.
    LocalIo,
    /// An object that was written over while it was read.
    Changed,
    /// The task of the fill panicked or was cancelled before it ended.
    Aborted,
}

/// The label of each [`FailedFill`], in the order of the enum.
const FAILED: [&str; 6] = [
    "hash_mismatch",
    "upstream_error",
    "short_body",
    "local_io",
    "changed",
    "aborted",
];

/// Why a create-only put that the upstream took was not kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SkippedKeep {
    /// No room: the object (or a body of no declared length, which cannot be reserved for) does
    /// not fit the budget or the critical mark.
    Budget,
    /// The stage directory or the cache directory could not be written.
    Io,
    /// Every keep the cache may run was running, or a fill of the key was.
    Busy,
    /// The upstream's HEAD after the put failed or did not describe the bytes that were sent, or
    /// the upstream did not read the body to its end.
    Head,
    /// The object was deleted while it was being kept.
    Deleted,
    /// The disk did not keep up with the body: the copy fell too far behind and was dropped,
    /// so that the upstream was never made to wait for the disk.
    Slow,
    /// The keep did not end within `keep_deadline` (a hung upstream or disk) and was abandoned.
    Deadline,
}

/// The label of each [`SkippedKeep`], in the order of the enum.
const SKIPPED_KEEP: [&str; 7] = [
    "budget", "io", "busy", "head", "deleted", "slow", "deadline",
];

/// Why an object was evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Evicted {
    /// The cache was over its high water mark.
    Budget,
    /// The file system was below its free-space floor.
    Floor,
}

/// The label of each [`Evicted`], in the order of the enum.
const EVICTED: [&str; 2] = ["budget", "floor"];

/// The kind of a family that only goes up (or is fixed once the process has started).
const COUNTER: &str = "counter";

/// The kind of a family that is measured now and goes up and down.
const GAUGE: &str = "gauge";

// The full name of each family, in the source as the page shows it, so that a name that the docs
// test or a reader looks for is found by search.
const HITS: &str = "tierstore_cache_hits_total";
const MISSES: &str = "tierstore_cache_misses_total";
const BYTES_SERVED_LOCAL: &str = "tierstore_cache_bytes_served_local_total";
const BYTES_SERVED_UPSTREAM: &str = "tierstore_cache_bytes_served_upstream_total";
const FILLS: &str = "tierstore_cache_fills_total";
const FILLS_SKIPPED: &str = "tierstore_cache_fills_skipped_total";
const FILLS_SKIPPED_BUSY: &str = "tierstore_cache_fills_skipped_busy_total";
const UPSTREAM_BYTES_READ: &str = "tierstore_cache_upstream_bytes_read_total";
const HEAD_HITS: &str = "tierstore_cache_head_hits_total";
const HEAD_MISSES: &str = "tierstore_cache_head_misses_total";
const UPSTREAM_PUTS: &str = "tierstore_cache_upstream_puts_total";
const UPSTREAM_PUT_BYTES: &str = "tierstore_cache_upstream_put_bytes_total";
const WRITES_THROUGH: &str = "tierstore_cache_writes_through_total";
const WRITE_EXISTS: &str = "tierstore_cache_write_exists_total";
const KEEPS_EXPIRED: &str = "tierstore_cache_keeps_expired_total";
const PINNED_SKIPS: &str = "tierstore_cache_pinned_skips_total";
const DISCARDED_AT_START: &str = "tierstore_cache_discarded_at_start_total";
pub(super) const BYTES: &str = "tierstore_cache_bytes";
pub(super) const OBJECTS: &str = "tierstore_cache_objects";
pub(super) const FS_FREE_BYTES: &str = "tierstore_cache_fs_free_bytes";
const STAGING_FILES: &str = "tierstore_cache_staging_files";
const FILL_FAILURES: &str = "tierstore_cache_fill_failures_total";
const UPSTREAM_READS: &str = "tierstore_cache_upstream_reads_total";
const UPSTREAM_PUT_FAILURES: &str = "tierstore_cache_upstream_put_failures_total";
const WRITE_KEEP_SKIPPED: &str = "tierstore_cache_write_keep_skipped_total";
const EVICTIONS: &str = "tierstore_cache_evictions_total";

/// The families that have one sample, with their kinds, in the order of the page.
pub(super) const FAMILIES: [(&str, &str); 21] = [
    (HITS, COUNTER),
    (MISSES, COUNTER),
    (BYTES_SERVED_LOCAL, COUNTER),
    (BYTES_SERVED_UPSTREAM, COUNTER),
    (FILLS, COUNTER),
    (FILLS_SKIPPED, COUNTER),
    (FILLS_SKIPPED_BUSY, COUNTER),
    (UPSTREAM_BYTES_READ, COUNTER),
    (HEAD_HITS, COUNTER),
    (HEAD_MISSES, COUNTER),
    (UPSTREAM_PUTS, COUNTER),
    (UPSTREAM_PUT_BYTES, COUNTER),
    (WRITES_THROUGH, COUNTER),
    (WRITE_EXISTS, COUNTER),
    (KEEPS_EXPIRED, COUNTER),
    (PINNED_SKIPS, COUNTER),
    (DISCARDED_AT_START, COUNTER),
    (BYTES, GAUGE),
    (OBJECTS, GAUGE),
    (FS_FREE_BYTES, GAUGE),
    (STAGING_FILES, GAUGE),
];

/// The families that have a sample for each label, with the label's name, the labels and the
/// kind, in the order of the page after [`FAMILIES`].
#[cfg(test)]
pub(super) const LABELLED: [(&str, &str, &[&str]); 5] = [
    (FILL_FAILURES, "reason", &FAILED),
    (
        UPSTREAM_READS,
        "op",
        &["get", "head", "list", "list_uploads"],
    ),
    (UPSTREAM_PUT_FAILURES, "reason", &PUT_FAILED),
    (WRITE_KEEP_SKIPPED, "reason", &SKIPPED_KEEP),
    (EVICTIONS, "reason", &EVICTED),
];

/// Why a put or a completion that reached the upstream was refused or failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PutFailed {
    /// The upstream's create-only put found the object (412).
    Exists,
    /// The upstream did not answer.
    Unavailable,
    /// The upstream had no room (507).
    Full,
    /// The body did not hash to the name of the object; the upstream stored nothing.
    BadDigest,
    /// Any other failure.
    Other,
}

impl PutFailed {
    /// The reason an error of a put counts under.
    pub(super) fn of(error: &crate::Error) -> Self {
        match error {
            crate::Error::AlreadyExists(_) => Self::Exists,
            crate::Error::Unavailable(_) => Self::Unavailable,
            crate::Error::Full(_) => Self::Full,
            crate::Error::BadDigest(_) => Self::BadDigest,
            _ => Self::Other,
        }
    }
}

/// The label of each [`PutFailed`], in the order of the enum.
const PUT_FAILED: [&str; 5] = ["exists", "unavailable", "full", "bad_digest", "other"];

/// What the cache has done since it started.
#[derive(Debug, Default)]
pub struct Counters {
    /// The GET calls the cache made to the upstream (a fill reads in ranged chunks: these are
    /// calls, not objects).
    pub(super) upstream_gets: AtomicU64,
    /// The HEAD calls the cache made to the upstream.
    pub(super) upstream_heads: AtomicU64,
    /// The listing calls (`list` and each page of `list_page`) the cache made to the upstream.
    pub(super) upstream_lists: AtomicU64,
    /// The calls of `list_uploads` the cache made to the upstream.
    pub(super) upstream_list_uploads: AtomicU64,
    /// The bytes the upstream gave to fills and pass-through reads.
    pub(super) upstream_bytes_read: AtomicU64,
    /// The HEADs the cache is asked that the index answered: the HEADs of clients, and the S3
    /// front's own HEAD before a GET it serves.
    pub(super) head_hits: AtomicU64,
    /// The same HEADs sent to the upstream.
    pub(super) head_misses: AtomicU64,
    /// The create-only puts and the completions of uploads the upstream accepted.
    pub(super) upstream_puts: AtomicU64,
    /// The bytes of those puts, and of the parts of uploads the upstream accepted.
    pub(super) upstream_put_bytes: AtomicU64,
    put_failures: [AtomicU64; PUT_FAILED.len()],
    pub(super) hits: AtomicU64,
    pub(super) misses: AtomicU64,
    pub(super) bytes_served_local: AtomicU64,
    pub(super) bytes_served_upstream: AtomicU64,
    pub(super) fills: AtomicU64,
    /// The fills not started or not kept for a reason that is not an error (no room, or busy).
    pub(super) fills_skipped: AtomicU64,
    /// Of `fills_skipped`, those skipped because every fill the cache may run was running.
    pub(super) fills_skipped_busy: AtomicU64,
    /// The create-only puts the upstream took.
    pub(super) writes_through: AtomicU64,
    /// The create-only puts answered `AlreadyExists`.
    pub(super) write_exists: AtomicU64,
    /// The puts answered before their keep ended, because it took longer than `keep_wait`.
    pub(super) keeps_expired: AtomicU64,
    keep_skips: [AtomicU64; SKIPPED_KEEP.len()],
    failures: [AtomicU64; FAILED.len()],
    evictions: [AtomicU64; EVICTED.len()],
    pub(super) pinned_skips: AtomicU64,
    pub(super) discarded_at_start: AtomicU64,
    /// The files in the stage directory now.
    pub(super) staging_files: AtomicU64,
}

/// One more of `counter`.
pub(super) fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_add(1, Ordering::Relaxed);
}

/// `by` more of `counter`.
pub(super) fn add(counter: &AtomicU64, by: u64) {
    let _ = counter.fetch_add(by, Ordering::Relaxed);
}

/// The value of `counter`.
pub(super) fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

impl Counters {
    /// The files removed at start: those the scan did not find whole, and what a fill that was
    /// running at the last stop left in the stage directory.
    #[must_use]
    pub fn discarded_at_start(&self) -> u64 {
        read(&self.discarded_at_start)
    }

    /// Counts a fill that failed for `why`.
    pub(super) fn failed(&self, why: FailedFill) {
        bump(&self.failures[why as usize]);
    }

    /// Counts a put that was taken by the upstream and not kept, for `why`.
    pub(super) fn keep_skipped(&self, why: SkippedKeep) {
        bump(&self.keep_skips[why as usize]);
    }

    /// The puts that were not kept for `why`.
    #[cfg(test)]
    pub(super) fn keeps_skipped(&self, why: SkippedKeep) -> u64 {
        read(&self.keep_skips[why as usize])
    }

    /// Counts a put or a completion the upstream refused, for the reason `error` gives.
    pub(super) fn put_failed(&self, error: &crate::Error) {
        bump(&self.put_failures[PutFailed::of(error) as usize]);
    }

    /// The puts and completions that failed for `why`.
    #[cfg(test)]
    pub(super) fn put_failures(&self, why: PutFailed) -> u64 {
        read(&self.put_failures[why as usize])
    }

    /// Counts an eviction for `why`.
    pub(super) fn evicted(&self, why: Evicted) {
        bump(&self.evictions[why as usize]);
    }

    /// The fills that failed for `why`.
    #[cfg(test)]
    pub(super) fn failures(&self, why: FailedFill) -> u64 {
        read(&self.failures[why as usize])
    }

    /// The evictions for `why`.
    #[cfg(test)]
    pub(super) fn evictions(&self, why: Evicted) -> u64 {
        read(&self.evictions[why as usize])
    }

    /// The text of `/metricz`, with the gauges the caller measured now.
    pub(super) fn render(
        &self,
        cache_bytes: u64,
        cache_objects: u64,
        fs_free_bytes: u64,
    ) -> String {
        let mut text = String::new();
        // Each family has one `# TYPE` line, before its first sample, as the OCI series have.
        let plain = [
            (HITS, read(&self.hits)),
            (MISSES, read(&self.misses)),
            (BYTES_SERVED_LOCAL, read(&self.bytes_served_local)),
            (BYTES_SERVED_UPSTREAM, read(&self.bytes_served_upstream)),
            (FILLS, read(&self.fills)),
            (FILLS_SKIPPED, read(&self.fills_skipped)),
            (FILLS_SKIPPED_BUSY, read(&self.fills_skipped_busy)),
            (UPSTREAM_BYTES_READ, read(&self.upstream_bytes_read)),
            (HEAD_HITS, read(&self.head_hits)),
            (HEAD_MISSES, read(&self.head_misses)),
            (UPSTREAM_PUTS, read(&self.upstream_puts)),
            (UPSTREAM_PUT_BYTES, read(&self.upstream_put_bytes)),
            (WRITES_THROUGH, read(&self.writes_through)),
            (WRITE_EXISTS, read(&self.write_exists)),
            (KEEPS_EXPIRED, read(&self.keeps_expired)),
            (PINNED_SKIPS, read(&self.pinned_skips)),
            // Set once, when the cache is opened, and the same for the rest of the process.
            (DISCARDED_AT_START, read(&self.discarded_at_start)),
            (BYTES, cache_bytes),
            (OBJECTS, cache_objects),
            (FS_FREE_BYTES, fs_free_bytes),
            (STAGING_FILES, read(&self.staging_files)),
        ];
        // The kinds are in `FAMILIES`, in the same order.
        for ((name, kind), (named, value)) in FAMILIES.iter().zip(plain) {
            debug_assert_eq!(*name, named);
            let _ = writeln!(text, "# TYPE {name} {kind}");
            let _ = writeln!(text, "{name} {value}");
        }
        let _ = writeln!(text, "# TYPE {FILL_FAILURES} {COUNTER}");
        for (label, counter) in FAILED.iter().zip(&self.failures) {
            let value = read(counter);
            let _ = writeln!(text, "{FILL_FAILURES}{{reason=\"{label}\"}} {value}");
        }
        let reads = [
            ("get", &self.upstream_gets),
            ("head", &self.upstream_heads),
            ("list", &self.upstream_lists),
            ("list_uploads", &self.upstream_list_uploads),
        ];
        let _ = writeln!(text, "# TYPE {UPSTREAM_READS} {COUNTER}");
        for (op, counter) in reads {
            let value = read(counter);
            let _ = writeln!(text, "{UPSTREAM_READS}{{op=\"{op}\"}} {value}");
        }
        let _ = writeln!(text, "# TYPE {UPSTREAM_PUT_FAILURES} {COUNTER}");
        for (label, counter) in PUT_FAILED.iter().zip(&self.put_failures) {
            let value = read(counter);
            let _ = writeln!(
                text,
                "{UPSTREAM_PUT_FAILURES}{{reason=\"{label}\"}} {value}"
            );
        }
        let _ = writeln!(text, "# TYPE {WRITE_KEEP_SKIPPED} {COUNTER}");
        for (label, counter) in SKIPPED_KEEP.iter().zip(&self.keep_skips) {
            let value = read(counter);
            let _ = writeln!(text, "{WRITE_KEEP_SKIPPED}{{reason=\"{label}\"}} {value}");
        }
        let _ = writeln!(text, "# TYPE {EVICTIONS} {COUNTER}");
        for (label, counter) in EVICTED.iter().zip(&self.evictions) {
            let value = read(counter);
            let _ = writeln!(text, "{EVICTIONS}{{reason=\"{label}\"}} {value}");
        }
        text
    }
}
