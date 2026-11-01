//! What the adapter does around a `Result` the core feeds to the scheduler, under the same
//! hold of the core's lock as the feed: the `action usage:` line, the invocation record's
//! `Ran` event (with the booking of the memory kill that ended the run, if one did) and the
//! node memory-pressure line. The farm's own driver does these before and after its feed;
//! the core runs [`Adapter::before_report`] and [`Adapter::after_report`] there, so no tick
//! or heartbeat can finish the operation between the two.
//!
//! What the host measured of the report ([`crate::action_usage::Measured`], the invocation
//! record's [`Run`]) is parked by [`KbfAdapter::stage_report`] in the `park` of
//! [`Core::report`](js_server_core::Core::report), which runs just before the hooks.

use js_server_core::{Report, Taken, View};
use kbf_invocation::Run;
use kbf_types::{Answer, Failure, Outcome, WorkerId};
use tonic::Status;

use super::adapter::KbfAdapter;
use crate::action_usage::{self, Measured, Subject};
use crate::{memory, run_record};

/// What a report measured, parked until its feed.
#[derive(Default)]
struct Staged {
    measured: Option<Measured>,
    run: Option<Run>,
}

/// What [`KbfAdapter::before_report`](js_server_core::Adapter::before_report) takes for
/// the matching `after_report`.
#[derive(Default)]
struct Awaiting {
    /// What the `action usage:` line says the action is, and what its run measured. Taken
    /// before the feed: a report that finishes the operation may drop its callers.
    usage: Option<(Subject, Measured)>,
    run: Option<Run>,
}

/// The adapter's state between the hooks of one report.
#[derive(Default)]
pub(super) struct ReportHooks {
    staged: Option<Staged>,
    awaiting: Option<Awaiting>,
}

impl KbfAdapter {
    /// Parks what the report being fed measured. Run it in the `park` of
    /// [`Core::report`](js_server_core::Core::report).
    pub(crate) fn stage_report(&mut self, measured: Option<Measured>, run: Option<Run>) {
        self.hooks.staged = Some(Staged { measured, run });
    }

    /// The work before the feed: finds what the `action usage:` line names.
    pub(super) fn note_report_start(
        &mut self,
        view: &View<'_>,
        worker: &WorkerId,
        report: &Report,
    ) {
        let Staged { measured, run } = self.hooks.staged.take().unwrap_or_default();
        // What the estimator will be shown of the report, if the scheduler takes it.
        let Report {
            operation,
            lease,
            outcome,
        } = *report;
        self.stage_learning(
            view.sched(),
            operation,
            lease,
            worker,
            outcome,
            measured.as_ref(),
        );
        let usage = measured.and_then(|measured| {
            let (_, waiter) = self.first_waiter(view.sched(), report.operation)?;
            Some((waiter.usage.clone(), measured))
        });
        self.hooks.awaiting = Some(Awaiting { usage, run });
    }

    /// The work after the feed, for a report the scheduler took (`taken`).
    pub(super) fn note_report_end(
        &mut self,
        view: &View<'_>,
        worker: &WorkerId,
        report: &Report,
        answers: &[Answer],
        taken: Taken,
    ) {
        let Awaiting { usage, run } = self.hooks.awaiting.take().unwrap_or_default();
        let learned = matches!(taken, Taken::Accepted | Taken::Held);
        self.finish_learning(view, report.lease, worker, learned);
        if taken != Taken::Accepted {
            return;
        }
        let Report {
            operation,
            lease,
            outcome,
        } = *report;
        if let Some((subject, measured)) = &usage {
            action_usage::log(operation, subject, measured, outcome, lease, worker);
        }
        if let Some(run) = run {
            let killed = match outcome {
                Outcome::Failed(Failure::OutOfMemory | Failure::NodeMemoryPressure) => answers
                    .iter()
                    .find(|answer| answer.lease == lease)
                    .map(|answer| answer.memory_runs.as_slice())
                    .or_else(|| view.sched().memory_runs(operation))
                    .and_then(<[_]>::last)
                    .filter(|kill| kill.worker == *worker),
                _ => None,
            };
            let run = match killed {
                Some(killed) => run_record::killed(run, killed),
                None => run,
            };
            self.recording.ran(operation, lease, run);
        }
        if outcome == Outcome::Failed(Failure::NodeMemoryPressure) {
            memory::pressure(worker, view.sched().memory_pressure(worker));
        }
    }

    /// Ends every worker stream with `status`, so each daemon dials again. Returns how many
    /// streams were still open.
    pub(crate) fn end_streams(&self, status: &Status) -> usize {
        self.nodes
            .values()
            .filter(|node| node.outbound.send(Err(status.clone())).is_ok())
            .count()
    }
}
