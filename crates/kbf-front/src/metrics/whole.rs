//! The scheduler's whole-node gauges: what the caps count and how many whole-machine
//! requests wait for each reason (`--whole-node-max`). They exist only when the option is
//! on: with it off nothing is rendered, so a scrape is what it was.

use std::sync::atomic::{AtomicBool, Ordering};

use kbf_sched::{WHOLE_REASONS, WholeReason, WholeSummary, whole_reason_name};

use super::{Families, Level};

/// The whole-node gauges, set by the farm each time it publishes its scheduler gauges.
#[derive(Debug, Default)]
pub struct WholeGauges {
    on: AtomicBool,
    held: Level,
    marks: Level,
    draining: Level,
    ceiling: Level,
    waiting: [Level; WHOLE_REASONS.len()],
}

fn slot(reason: WholeReason) -> usize {
    WHOLE_REASONS
        .iter()
        .position(|r| *r == reason)
        .unwrap_or_default()
}

impl WholeGauges {
    /// Sets every gauge from the scheduler's summary; `None` (the option is off) leaves
    /// them unrendered.
    pub fn set(&self, summary: Option<&WholeSummary>) {
        let Some(summary) = summary else {
            self.on.store(false, Ordering::Relaxed);
            return;
        };
        let level = |n: usize| n as u64;
        self.held.set(level(summary.held));
        self.marks.set(level(summary.marks));
        self.draining.set(level(summary.draining));
        self.ceiling.set(level(summary.ceiling));
        for reason in WHOLE_REASONS {
            let count = summary.waiting.get(&reason).copied().unwrap_or(0);
            self.waiting[slot(reason)].set(level(count));
        }
        self.on.store(true, Ordering::Relaxed);
    }

    /// Whether the gauges are rendered: the option is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// The queued whole-machine requests waiting for `reason`.
    #[must_use]
    pub fn waiting(&self, reason: WholeReason) -> u64 {
        self.waiting[slot(reason)].get()
    }

    /// Workers held by a whole-machine lease.
    #[must_use]
    pub fn held(&self) -> u64 {
        self.held.get()
    }

    /// Workers held by a queued request's mark.
    #[must_use]
    pub fn marks(&self) -> u64 {
        self.marks.get()
    }

    /// Marked workers that still have leases.
    #[must_use]
    pub fn draining(&self) -> u64 {
        self.draining.get()
    }

    /// The most workers that may be whole now.
    #[must_use]
    pub fn ceiling(&self) -> u64 {
        self.ceiling.get()
    }

    pub(super) fn render(&self, out: &mut Families) {
        if !self.is_on() {
            return;
        }
        out.gauge(
            "kbf_sched_whole_held",
            "Workers held by a whole-machine lease, as of the last scheduler input",
            self.held(),
        );
        out.gauge(
            "kbf_sched_whole_marks",
            "Workers held by a queued whole-machine request while they drain, as of the last \
             scheduler input",
            self.marks(),
        );
        out.gauge(
            "kbf_sched_whole_draining",
            "Marked workers that still have leases, which --whole-node-max-draining bounds",
            self.draining(),
        );
        out.gauge(
            "kbf_sched_whole_ceiling",
            "The most workers that may be whole now: --whole-node-max, or the capable workers \
             less those kept for ordinary work if that is fewer",
            self.ceiling(),
        );
        let name = "kbf_sched_whole_waiting";
        out.head(
            name,
            "gauge",
            "Queued whole-machine requests, by the reason each waits, as of the last scheduler \
             input",
        );
        for reason in WHOLE_REASONS {
            let _ = std::fmt::Write::write_fmt(
                &mut out.0,
                format_args!(
                    "{name}{{reason=\"{}\"}} {}\n",
                    whole_reason_name(reason),
                    self.waiting(reason)
                ),
            );
        }
    }
}
