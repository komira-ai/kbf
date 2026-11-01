//! Where the time of one executor run goes, as the executor itself sees it.
//!
//! A run is stamped, on the executor's own clock, at the moments it passes: its `Start` first
//! seen, its run prepared, its `POST` accepted, the run first read back as running, and read
//! back as ended, and its ingest done. The time between two stamps is a phase, observed into
//! `kbf_executor_phase_seconds{phase=...}` the moment the second stamp is set. A phase whose
//! first stamp was never set (the run was not seen running, the run never started) is not
//! observed: it is absent, never zero.
//!
//! What the stamps can know is bounded by how the executor learns. The lease server's `Run`
//! has no timestamps, and it learns a run's state from the agent's heartbeat, which trails the
//! host by one or two beats; the executor reads the host's runs once per pass. So `queue` and
//! `task` are the executor's passes that first saw the run running and ended, not the instants
//! the host started and finished it: each is good to a pass plus a heartbeat or two, and a run
//! that starts and ends between two reads is seen ended without having been seen running (it has
//! a `run` phase and no `queue` or `task`).
//!
//! The phases:
//!
//! - `prepare`: the `Start` first seen to its run being ready (the CAS read, the key, the url
//!   and the grant);
//! - `start`: ready to the lease server accepting the `POST` (the lease renewal if one was
//!   needed, and every pass the lease server answered "no capacity" or "not ready");
//! - `queue`: accepted to first read back as running;
//! - `task`: first read back as running to read back as exited or ended;
//! - `run`: accepted to read back as exited or ended, seen or not seen running in between
//!   (`queue` plus `task` when both are observed);
//! - `ingest`: read back as ended to the frontend's read of the pack done.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::executor::metrics::Metrics;

/// One stretch of a run's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Prepare,
    Start,
    Queue,
    Task,
    Run,
    Ingest,
}

impl Phase {
    /// Every phase, in the order the metric lists them.
    pub(crate) const ALL: [Self; 6] = [
        Self::Prepare,
        Self::Start,
        Self::Queue,
        Self::Task,
        Self::Run,
        Self::Ingest,
    ];

    /// The `phase` label.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Start => "start",
            Self::Queue => "queue",
            Self::Task => "task",
            Self::Run => "run",
            Self::Ingest => "ingest",
        }
    }

    const fn slot(self) -> usize {
        self as usize
    }
}

/// A moment of a run that is stamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The `Start` first acted on.
    Seen,
    /// The run is ready to post.
    Prepared,
    /// The lease server accepted the `POST`.
    Posted,
    /// The run first read back as running.
    Running,
    /// The run read back as exited or ended.
    Ended,
    /// The frontend's read of the pack is done.
    Done,
}

/// Each phase, with the stamp that begins it and the stamp that ends it.
const PHASES: [(Phase, Mark, Mark); 6] = [
    (Phase::Prepare, Mark::Seen, Mark::Prepared),
    (Phase::Start, Mark::Prepared, Mark::Posted),
    (Phase::Queue, Mark::Posted, Mark::Running),
    (Phase::Task, Mark::Running, Mark::Ended),
    (Phase::Run, Mark::Posted, Mark::Ended),
    (Phase::Ingest, Mark::Ended, Mark::Done),
];

/// The stamps of one run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Timeline {
    marks: [Option<Instant>; 6],
}

impl Timeline {
    /// Stamps `mark` at `at` if it has no stamp yet, and returns the phases that ends: those
    /// whose first stamp is set. A stamp that is already set is kept and ends nothing.
    pub(crate) fn mark(&mut self, mark: Mark, at: Instant) -> Vec<(Phase, Duration)> {
        let slot = &mut self.marks[mark as usize];
        if slot.is_some() {
            return Vec::new();
        }
        *slot = Some(at);
        PHASES
            .iter()
            .filter(|(_, _, end)| *end == mark)
            .filter_map(|(phase, begin, _)| {
                let begun = self.marks[*begin as usize]?;
                Some((*phase, at.saturating_duration_since(begun)))
            })
            .collect()
    }

    /// As [`Timeline::mark`], and observes what it returns into `metrics`.
    pub(crate) fn stamp(&mut self, mark: Mark, at: Instant, metrics: &Metrics) {
        for (phase, took) in self.mark(mark, at) {
            metrics.phase_took(phase, took);
        }
    }
}

/// The upper bounds of the buckets, in microseconds, with the `le` label of each.
const BOUNDS: [(u64, &str); 12] = [
    (5_000, "0.005"),
    (25_000, "0.025"),
    (100_000, "0.1"),
    (500_000, "0.5"),
    (1_000_000, "1"),
    (2_500_000, "2.5"),
    (5_000_000, "5"),
    (10_000_000, "10"),
    (30_000_000, "30"),
    (120_000_000, "120"),
    (600_000_000, "600"),
    (3_600_000_000, "3600"),
];

/// `kbf_executor_phase_seconds`: a histogram of the phases, one series per phase.
#[derive(Debug, Default)]
pub(crate) struct PhaseHistograms {
    /// Per phase, the observations that fell in each bucket (the last is over every bound).
    buckets: [[AtomicU64; 13]; 6],
    micros: [AtomicU64; 6],
}

impl PhaseHistograms {
    /// Counts one `phase` of `took`.
    pub(crate) fn observe(&self, phase: Phase, took: Duration) {
        let micros = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        let bucket = BOUNDS
            .iter()
            .position(|(bound, _)| micros <= *bound)
            .unwrap_or(BOUNDS.len());
        self.buckets[phase.slot()][bucket].fetch_add(1, Ordering::Relaxed);
        self.micros[phase.slot()].fetch_add(micros, Ordering::Relaxed);
    }

    /// Appends the family to `out`, in the Prometheus text format.
    pub(crate) fn render(&self, out: &mut String) {
        let name = "kbf_executor_phase_seconds";
        let _ = writeln!(
            out,
            "# HELP {name} How long the phases of an executor run took, by phase; each \
             phase is between two moments the executor saw, not the host's own instants\n\
             # TYPE {name} histogram"
        );
        for phase in Phase::ALL {
            let label = phase.label();
            let mut seen = 0;
            let bounds = BOUNDS.iter().map(|(_, le)| *le).chain(["+Inf"]);
            for (le, count) in bounds.zip(&self.buckets[phase.slot()]) {
                seen += count.load(Ordering::Relaxed);
                let _ = writeln!(out, "{name}_bucket{{phase=\"{label}\",le=\"{le}\"}} {seen}");
            }
            let micros = self.micros[phase.slot()].load(Ordering::Relaxed);
            let (whole, part) = (micros / 1_000_000, micros % 1_000_000);
            let _ = writeln!(out, "{name}_sum{{phase=\"{label}\"}} {whole}.{part:06}");
            let _ = writeln!(out, "{name}_count{{phase=\"{label}\"}} {seen}");
        }
    }
}

#[cfg(test)]
#[path = "phases_tests.rs"]
mod tests;
