//! Where the time of one executor run goes, as the executor itself sees it.
//!
//! A run is stamped, on the executor's own clock, at the moments it reaches: the Execute that
//! made its operation (for a later attempt, the run before it ending), its job prepared, its
//! submit answered with the job, the job first read back as running, and read back as ended,
//! its ingest done, and its callers answered. The time between two stamps is a phase, observed into
//! `kbf_executor_phase_seconds{phase=...}` the moment the second stamp is set. A phase whose
//! first stamp was never set (the run was not seen running, the run was never submitted) is not
//! observed: it is absent, never zero.
//!
//! What the stamps can know is bounded by how the executor learns. The stamps are the
//! executor's own clock, not the job's times, and the service learns a job's state from its
//! host's agent, which trails the host by one or two beats; the executor reads each job once
//! per poll. So `queue` and `task` are the executor's polls that first saw the job running and
//! ended, not the instants the host started and finished it: each is good to a poll plus a
//! heartbeat or two, and a job that starts and ends between two reads is seen ended without
//! having been seen running (it has a `run` phase and no `queue` or `task`).
//!
//! The phases:
//!
//! - `prepare`: the Execute to its job being ready (the CAS read, the put directory and the
//!   action tree's name);
//! - `start`: ready to the job service answering the submit with the job (the wait for the tree's
//!   writes, with the operation's row written meanwhile, then each submit, including each one that
//!   was not answered and was sent again);
//! - `queue`: accepted to first read back as running (queued, placed and starting);
//! - `task`: first read back as running to read back as ended;
//! - `run`: accepted to read back as ended, seen or not seen running in between
//!   (`queue` plus `task` when both are observed);
//! - `ingest`: read back as ended to the frontend's read of the pack done;
//! - `submit`: the Execute to the job service answering the submit with the job (`prepare`
//!   plus `start`);
//! - `execute`: the Execute to its callers answered, for the run that answers them.

use std::time::Duration;

use tokio::time::Instant;

use crate::executor::histogram::{Histograms, Series};
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
    Submit,
    Execute,
}

impl Phase {
    /// Every phase, in the order the metric lists them.
    pub(crate) const ALL: [Self; 8] = [
        Self::Prepare,
        Self::Start,
        Self::Queue,
        Self::Task,
        Self::Run,
        Self::Ingest,
        Self::Submit,
        Self::Execute,
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
            Self::Submit => "submit",
            Self::Execute => "execute",
        }
    }
}

/// A moment of a run that is stamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The Execute that made the operation, or the end of the run before this attempt.
    Seen,
    /// The job is ready to submit.
    Prepared,
    /// The job service answered the submit with the job.
    Posted,
    /// The job first read back as running.
    Running,
    /// The job read back as ended.
    Ended,
    /// The frontend's read of the pack is done.
    Done,
    /// The operation's callers are answered.
    Answered,
}

/// Each phase, with the stamp that begins it and the stamp that ends it.
const PHASES: [(Phase, Mark, Mark); 8] = [
    (Phase::Prepare, Mark::Seen, Mark::Prepared),
    (Phase::Start, Mark::Prepared, Mark::Posted),
    (Phase::Queue, Mark::Posted, Mark::Running),
    (Phase::Task, Mark::Running, Mark::Ended),
    (Phase::Run, Mark::Posted, Mark::Ended),
    (Phase::Ingest, Mark::Ended, Mark::Done),
    (Phase::Submit, Mark::Seen, Mark::Posted),
    (Phase::Execute, Mark::Seen, Mark::Answered),
];

/// The stamps of one run.
#[derive(Clone, Debug, Default)]
pub(crate) struct Timeline {
    marks: [Option<Instant>; 7],
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

impl Series for Phase {
    const NAME: &'static str = "kbf_executor_phase_seconds";
    const HELP: &'static str = "How long the phases of an executor run took, by phase; each \
                                phase is between two moments the executor saw, not the host's \
                                own instants";
    const LABEL: &'static str = "phase";
    const ALL: &'static [Self] = &Self::ALL;

    fn label(self) -> &'static str {
        Self::label(self)
    }

    fn slot(self) -> usize {
        self as usize
    }
}

/// `kbf_executor_phase_seconds`: a histogram of the phases, one series per phase, with the
/// buckets of [`crate::executor::histogram::BOUNDS`].
pub(crate) type PhaseHistograms = Histograms<Phase>;

#[cfg(test)]
#[path = "phases_tests.rs"]
mod tests;
