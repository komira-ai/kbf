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
    /// The upstream's HEAD after the put failed or did not describe the bytes that were sent.
    Head,
    /// The object was deleted while it was being kept.
    Deleted,
}

/// The label of each [`SkippedKeep`], in the order of the enum.
const SKIPPED_KEEP: [&str; 5] = ["budget", "io", "busy", "head", "deleted"];

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

/// What the cache has done since it started.
#[derive(Debug, Default)]
pub struct Counters {
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
        let plain = [
            ("hits_total", read(&self.hits)),
            ("misses_total", read(&self.misses)),
            ("bytes_served_local_total", read(&self.bytes_served_local)),
            (
                "bytes_served_upstream_total",
                read(&self.bytes_served_upstream),
            ),
            ("fills_total", read(&self.fills)),
            ("fills_skipped_total", read(&self.fills_skipped)),
            ("fills_skipped_busy_total", read(&self.fills_skipped_busy)),
            ("writes_through_total", read(&self.writes_through)),
            ("write_exists_total", read(&self.write_exists)),
            ("pinned_skips_total", read(&self.pinned_skips)),
            ("discarded_at_start_total", read(&self.discarded_at_start)),
            ("bytes", cache_bytes),
            ("objects", cache_objects),
            ("fs_free_bytes", fs_free_bytes),
            ("staging_files", read(&self.staging_files)),
        ];
        for (name, value) in plain {
            let _ = writeln!(text, "tierstore_cache_{name} {value}");
        }
        for (label, counter) in FAILED.iter().zip(&self.failures) {
            let value = read(counter);
            let _ = writeln!(
                text,
                "tierstore_cache_fill_failures_total{{reason=\"{label}\"}} {value}"
            );
        }
        for (label, counter) in SKIPPED_KEEP.iter().zip(&self.keep_skips) {
            let value = read(counter);
            let _ = writeln!(
                text,
                "tierstore_cache_write_keep_skipped_total{{reason=\"{label}\"}} {value}"
            );
        }
        for (label, counter) in EVICTED.iter().zip(&self.evictions) {
            let value = read(counter);
            let _ = writeln!(
                text,
                "tierstore_cache_evictions_total{{reason=\"{label}\"}} {value}"
            );
        }
        text
    }
}
