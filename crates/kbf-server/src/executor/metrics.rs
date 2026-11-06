//! What the executor counts, for `GET /metrics`: how the runs ended, what the ingest made of
//! the packs, the `Start`s it dropped as late, and what it asked of the job service: the jobs
//! submitted, those submitted again after an answer was lost, those cancelled, those of an
//! earlier process cancelled at start, and the jobs seen on a host that missed its grace.
//!
//! The counters are shared by the executor and the page that renders them. A server that never
//! builds an executor builds none of this and renders none of it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_proto::worker::{self, Lost, MemoryKill};
use tonic::Code;

use crate::executor::RejectReason;
use crate::executor::phases::{Phase, PhaseHistograms};

/// How a run ended, as the farm was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// The action ran and the frontend rebuilt its result.
    Ran,
    /// An ending the farm runs again.
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

    /// How a `Result` that is about to be sent ends its run.
    pub(crate) fn of(result: &worker::Result) -> Self {
        let code = result.status.as_ref().map(|status| status.code);
        if result.action_result.is_some() {
            Self::Ran
        } else if result.lost != Lost::Unspecified as i32 {
            Self::Lost
        } else if result.memory_kill != MemoryKill::Unspecified as i32 {
            Self::OutOfMemory
        } else if code == Some(Code::DeadlineExceeded as i32) {
            Self::Timeout
        } else if code == Some(Code::InvalidArgument as i32) {
            Self::Invalid
        } else {
            Self::Infra
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
    /// The ingest panicked or was cancelled.
    Stopped,
}

impl IngestEnd {
    fn labels() -> impl Iterator<Item = &'static str> {
        ["ok", "unavailable", "stopped"]
            .into_iter()
            .chain(REASONS.iter().map(|(_, label)| *label))
    }

    fn slot(self) -> usize {
        match self {
            Self::Ok => 0,
            Self::Unavailable => 1,
            Self::Stopped => 2,
            Self::Rejected(reason) => {
                3 + REASONS
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
    ingests: [AtomicU64; 12],
    late_starts: AtomicU64,
    jobs_submitted: AtomicU64,
    jobs_resubmitted: AtomicU64,
    jobs_cancelled: AtomicU64,
    jobs_adopted_cancelled: AtomicU64,
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

    pub(crate) fn late_start(&self) {
        self.late_starts.fetch_add(1, Ordering::Relaxed);
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

    /// The job service answered a cancel of a job of this process.
    pub(crate) fn job_cancelled(&self) {
        self.jobs_cancelled.fetch_add(1, Ordering::Relaxed);
    }

    /// The job service answered a cancel of a job an earlier process left.
    pub(crate) fn job_adopted_cancelled(&self) {
        self.jobs_adopted_cancelled.fetch_add(1, Ordering::Relaxed);
    }

    /// A job was seen on a host that has missed its grace.
    pub(crate) fn host_silence(&self) {
        self.host_silence_seen.fetch_add(1, Ordering::Relaxed);
    }

    /// Appends the families to `out`, in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP kbf_executor_runs_total Runs the executor ended, by how the farm was told\n\
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
                "kbf_executor_late_starts_total",
                "Starts dropped because their window ended before their job was submitted",
                &self.late_starts,
            ),
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
                "kbf_executor_jobs_cancelled_total",
                "Cancels of this process's jobs the job service answered",
                &self.jobs_cancelled,
            ),
            (
                "kbf_executor_jobs_adopted_cancelled_total",
                "Live jobs of an earlier process under this frontend's id cancelled at start",
                &self.jobs_adopted_cancelled,
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
