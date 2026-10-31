//! What the executor counts, for `GET /metrics`: the hosts it holds, how the runs ended, what
//! the ingest made of the packs, the `Start`s it dropped as late, and the leases it released
//! that no slot held.
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
    /// Lost to the host or the lease server.
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
    held: AtomicU64,
    runs: [AtomicU64; 6],
    ingests: [AtomicU64; 12],
    late_starts: AtomicU64,
    strays_released: AtomicU64,
    phases: PhaseHistograms,
}

impl Metrics {
    /// The hosts held now.
    pub(crate) fn hold(&self, hosts: usize) {
        self.held.store(hosts as u64, Ordering::Relaxed);
    }

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

    pub(crate) fn stray_released(&self) {
        self.strays_released.fetch_add(1, Ordering::Relaxed);
    }

    /// The hosts held, as last stepped.
    #[must_use]
    pub fn held(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    /// Appends the families to `out`, in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let held = self.held();
        let _ = writeln!(
            out,
            "# HELP kbf_executor_leases_held Host leases the executor holds now\n\
             # TYPE kbf_executor_leases_held gauge\nkbf_executor_leases_held {held}"
        );
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
                "Starts dropped because their window ended before the run was posted",
                &self.late_starts,
            ),
            (
                "kbf_executor_stray_leases_released_total",
                "Leases released because no slot of the executor held them",
                &self.strays_released,
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
