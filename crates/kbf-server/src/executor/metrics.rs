//! What the executor counts, for `GET /metrics`: how the runs ended, what the ingest made of
//! the packs, and what it asked of the job service: the jobs submitted, those submitted again
//! after an answer was lost, the operations run again as a new attempt, the jobs cancelled for
//! their callers, the operations taken up from the state store, and the jobs seen on a host that
//! missed its grace.
//!
//! The counters are shared by the executor and the page that renders them. A server that never
//! builds an executor builds none of this and renders none of it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::executor::RejectReason;
use crate::executor::outcome::Settled;
use crate::executor::phases::{Phase, PhaseHistograms};

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// The action ran and the frontend rebuilt its result.
    Ran,
    /// An ending for infrastructure reasons: the operation runs again while it has attempts left.
    Infra,
    /// The action's own timeout.
    Timeout,
    /// Killed at its own memory limit.
    OutOfMemory,
    /// Lost to the host or the job service.
    Lost,
    /// An action that cannot run here.
    Invalid,
}

impl RunEnd {
    const ALL: [Self; 6] = [
        Self::Ran,
        Self::Infra,
        Self::Timeout,
        Self::OutOfMemory,
        Self::Lost,
        Self::Invalid,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Ran => "ran",
            Self::Infra => "infra",
            Self::Timeout => "timeout",
            Self::OutOfMemory => "out_of_memory",
            Self::Lost => "lost",
            Self::Invalid => "invalid",
        }
    }

    /// How a run that produced no result ends.
    pub(crate) const fn of(settled: &Settled) -> Self {
        match settled {
            Settled::Infra(_) => Self::Infra,
            Settled::Timeout => Self::Timeout,
            Settled::OutOfMemory => Self::OutOfMemory,
            Settled::Lost(_) => Self::Lost,
            Settled::Invalid(_) => Self::Invalid,
        }
    }
}

/// What the ingest of a task's pack came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IngestEnd {
    /// The pack was read and the result rebuilt.
    Ok,
    /// The pack was refused, for a reason.
    Rejected(RejectReason),
    /// The store could not be read.
    Unavailable,
}

impl IngestEnd {
    fn labels() -> impl Iterator<Item = &'static str> {
        ["ok", "unavailable"]
            .into_iter()
            .chain(REASONS.iter().map(|(_, label)| *label))
    }

    fn slot(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::Unavailable => 1,
            Self::Rejected(reason) => {
                2 + REASONS
                    .iter()
                    .position(|(r, _)| *r == reason)
                    .unwrap_or_default()
            }
        }
    }
}

/// Every reason a pack is refused, with its label.
const REASONS: [(RejectReason, &str); 9] = [
    (RejectReason::Missing, "rejected_missing"),
    (RejectReason::WrongKey, "rejected_wrong_key"),
    (RejectReason::TooLarge, "rejected_too_large"),
    (RejectReason::Malformed, "rejected_malformed"),
    (RejectReason::HashMismatch, "rejected_hash_mismatch"),
    (RejectReason::BadManifest, "rejected_bad_manifest"),
    (RejectReason::Incomplete, "rejected_incomplete"),
    (RejectReason::Expired, "rejected_expired"),
    (RejectReason::Claimed, "rejected_claimed"),
];

/// The executor's counters.
#[derive(Debug, Default)]
pub struct Metrics {
    runs: [AtomicU64; 6],
    ingests: [AtomicU64; 11],
    jobs_submitted: AtomicU64,
    jobs_resubmitted: AtomicU64,
    attempts: AtomicU64,
    jobs_cancelled: AtomicU64,
    reattached: AtomicU64,
    host_silence_seen: AtomicU64,
    phases: PhaseHistograms,
}

impl Metrics {
    pub(crate) fn run_ended(&self, end: RunEnd) {
        let at = RunEnd::ALL
            .iter()
            .position(|e| *e == end)
            .unwrap_or_default();
        self.runs[at].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn ingested(&self, end: IngestEnd) {
        self.ingests[end.slot()].fetch_add(1, Ordering::Relaxed);
    }

    /// One `phase` of a run took `took`.
    pub(crate) fn phase_took(&self, phase: Phase, took: std::time::Duration) {
        self.phases.observe(phase, took);
    }

    /// The job service answered a submit with the job.
    pub(crate) fn job_submitted(&self) {
        self.jobs_submitted.fetch_add(1, Ordering::Relaxed);
    }

    /// A submit is sent again because the answer to the one before was lost.
    pub(crate) fn job_resubmitted(&self) {
        self.jobs_resubmitted.fetch_add(1, Ordering::Relaxed);
    }

    /// An operation is run again as its next attempt, after a run that ended for an
    /// infrastructure reason of its own.
    pub(crate) fn attempt_resubmitted(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// The job service answered a cancel of a job, asked for by its operation's callers.
    pub(crate) fn job_cancelled(&self) {
        self.jobs_cancelled.fetch_add(1, Ordering::Relaxed);
    }

    /// An unfinished operation another process left in the state store was taken up.
    pub(crate) fn reattached(&self) {
        self.reattached.fetch_add(1, Ordering::Relaxed);
    }

    /// A job was seen on a host that has missed its grace.
    pub(crate) fn host_silence(&self) {
        self.host_silence_seen.fetch_add(1, Ordering::Relaxed);
    }

    /// Appends the families to `out`, in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP kbf_executor_runs_total Runs the executor ended, by how they ended\n\
             # TYPE kbf_executor_runs_total counter"
        );
        for (end, count) in RunEnd::ALL.iter().zip(&self.runs) {
            let _ = writeln!(
                out,
                "kbf_executor_runs_total{{outcome=\"{}\"}} {}",
                end.label(),
                count.load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP kbf_executor_ingests_total What the ingest of a task's pack came to\n\
             # TYPE kbf_executor_ingests_total counter"
        );
        for (label, count) in IngestEnd::labels().zip(&self.ingests) {
            let _ = writeln!(
                out,
                "kbf_executor_ingests_total{{result=\"{label}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
        let counters = [
            (
                "kbf_executor_jobs_submitted_total",
                "Submits the job service answered with the job",
                &self.jobs_submitted,
            ),
            (
                "kbf_executor_jobs_resubmitted_after_lost_answer_total",
                "Submits sent again, with the same job id, because the answer to the one before was lost",
                &self.jobs_resubmitted,
            ),
            (
                "kbf_executor_attempts_resubmitted_total",
                "Operations run again as a new attempt after a run that ended for an infrastructure reason",
                &self.attempts,
            ),
            (
                "kbf_executor_jobs_cancelled_total",
                "Cancels of jobs, asked for by their operation's callers, the job service answered",
                &self.jobs_cancelled,
            ),
            (
                "kbf_executor_operations_reattached_total",
                "Unfinished operations another process left in the state store, taken up",
                &self.reattached,
            ),
            (
                "kbf_executor_host_silence_seen_total",
                "Jobs seen on a host that has missed its grace, once a job",
                &self.host_silence_seen,
            ),
        ];
        for (name, help, count) in counters {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
                count.load(Ordering::Relaxed)
            );
        }
        self.phases.render(out);
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
