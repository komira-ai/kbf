//! An Execute's time before its first stream item: the window [`Presubmit`] that measures it,
//! and the split of it ([`PresubmitSplit`]) the [`Submission`](super::Submission) carries to
//! the server's per-job line.
//!
//! The split has the stages the front runs one after the other, each timed on its own:
//! authorization, the action-cache lookup (beside the Action's read), the Command's read beside
//! the input walk, and, inside that walk, its Directory phase and its presence checks, with
//! their counts. What the front does after the walk (the Action's and Command's decoding, the
//! dispatcher's id for the operation) is not a field: the per-job line names it as the rest of
//! `presubmit_ms`.

use std::time::Duration;

use super::{Arrival, PRESUBMIT_WARN};
use crate::input_tree::Walked;
use crate::metrics::CacheMetrics;

/// Where an Execute spent its time at the front before it was submitted, as the front measured
/// it; all zero for a stage the Execute did not reach. For the server's per-job line only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresubmitSplit {
    /// From the arrival ([`Arrival`]) to the call's authorization.
    pub auth: Duration,
    /// The action-cache lookup and the Action's read, which run together.
    pub lookup: Duration,
    /// The Command's read and the input walk, which run together.
    pub inputs: Duration,
    /// The input walk alone.
    pub walk: Duration,
    /// Of a walk over a store: its Directory phase (the Directories read, or taken from the
    /// content cache), then its presence checks (FindMissing of the files and of the cached
    /// Directories); the two run one after the other.
    pub walk_dirs: Duration,
    pub walk_files: Duration,
    /// Directory blobs the walk read from the store.
    pub dir_reads: u64,
    /// Directories skipped with everything under them, as verified by an earlier walk.
    pub dirs_skipped: u64,
    /// Directories taken without a read; of them, the cached ones put to FindMissing.
    pub dir_cache_hits: u64,
    pub dirs_present_checked: u64,
    /// Files whose presence was asked about, and the FindMissing calls that took.
    pub files_checked: u64,
    pub file_asks: u64,
}

impl PresubmitSplit {
    /// Adds the figures of `walked`, a walk of the Execute's input tree.
    pub(super) fn walked(&mut self, walked: &Walked) {
        self.dir_reads += walked.dirs_read;
        self.dirs_skipped += walked.dirs_skipped;
        self.dir_cache_hits += walked.dir_cache_hits;
        self.dirs_present_checked += walked.dirs_present_checked;
        self.walk_dirs += walked.dirs_time;
        self.walk_files += walked.files_time;
        self.file_asks += walked.file_asks;
        self.files_checked += walked.files_checked;
    }
}

/// One Execute's time before its first stream item, from the handler's start: observed
/// in `kbf_execute_presubmit_seconds` when dropped, whether the call answered, failed
/// or was dropped by its caller (a stalled call the client gave up on is the one the
/// operator needs to see), and logged at `WARN` when it took [`PRESUBMIT_WARN`] or more. Its
/// start is also the [`Submission`](super::Submission)'s [`Arrival`].
pub(super) struct Presubmit<'a> {
    metrics: &'a CacheMetrics,
    /// The handler's start, which the submission carries as its [`Arrival`].
    pub(super) arrival: Arrival,
    /// What the call is about, once its request is read; empty before (a call denied or
    /// dropped while authorizing).
    pub(super) action: String,
    pub(super) instance: String,
    pub(super) invocation: String,
    /// The stages before the submit, which the submission carries too.
    pub(super) split: PresubmitSplit,
    /// The dispatcher's submit.
    pub(super) submit: Duration,
    _in_flight: crate::metrics::Raised<'a>,
}

impl<'a> Presubmit<'a> {
    pub(super) fn begin(metrics: &'a CacheMetrics) -> Self {
        Self {
            metrics,
            arrival: Arrival::now(),
            action: String::new(),
            instance: String::new(),
            invocation: String::new(),
            split: PresubmitSplit::default(),
            submit: Duration::ZERO,
            _in_flight: metrics.execute_presubmit_in_flight.raise(),
        }
    }
}

impl Drop for Presubmit<'_> {
    fn drop(&mut self) {
        let elapsed = self.arrival.at.elapsed();
        self.metrics.execute_presubmit.observe_duration(elapsed);
        if elapsed < PRESUBMIT_WARN {
            return;
        }
        let split = &self.split;
        tracing::warn!(
            action = %self.action,
            instance = %self.instance,
            invocation = %self.invocation,
            presubmit_ms = elapsed.as_millis(),
            walk_ms = split.walk.as_millis(),
            submit_ms = self.submit.as_millis(),
            dir_reads = split.dir_reads,
            dirs_skipped = split.dirs_skipped,
            dir_cache_hits = split.dir_cache_hits,
            dirs_present_checked = split.dirs_present_checked,
            dirs_ms = split.walk_dirs.as_millis(),
            files_ms = split.walk_files.as_millis(),
            file_asks = split.file_asks,
            files_checked = split.files_checked,
            "Execute spent {PRESUBMIT_WARN:?} or more before its first operation"
        );
    }
}
