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

/// The stamps of one run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Timeline {
    marks: [Option<Instant>; 6],
}

impl Timeline {
    /// Stamps `mark` at `at` if it has no stamp yet, and returns the phases that ends.
    pub(crate) fn mark(&mut self, mark: Mark, at: Instant) -> Vec<(Phase, Duration)> {
        let _ = (mark, at);
        Vec::new()
    }

    /// As [`Timeline::mark`], and observes what it returns into `metrics`.
    pub(crate) fn stamp(&mut self, mark: Mark, at: Instant, metrics: &Metrics) {
        let _ = (mark, at, metrics);
    }
}

/// `kbf_executor_phase_seconds`: a histogram of the phases, one series per phase.
#[derive(Debug, Default)]
pub(crate) struct PhaseHistograms {}

impl PhaseHistograms {
    /// Counts one `phase` of `took`.
    pub(crate) fn observe(&self, phase: Phase, took: Duration) {
        let _ = (phase, took);
    }

    /// Appends the family to `out`, in the Prometheus text format.
    pub(crate) fn render(&self, out: &mut String) {
        let _ = out;
    }
}

#[cfg(test)]
#[path = "phases_tests.rs"]
mod tests;
