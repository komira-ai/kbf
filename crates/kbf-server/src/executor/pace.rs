//! How soon an operation reads its job again while the job has not started.
//!
//! The first read of a job is sent as soon as it is submitted (or taken up). While the service
//! says the job is queued, placed or starting, the next read waits [`FIRST_STEP`], then twice
//! that, doubling up to the poll ([`Config::poll`](super::Config::poll)); once the job is read
//! running, or the steps have reached the poll, every read waits the poll, and the steps do not
//! start again for that job. A job that starts at once is so seen running without a poll's wait,
//! and a job that waits for a host is read at most `1 + ceil(log2(poll / FIRST_STEP))` times
//! before its reads are a poll apart: 6 at the default poll of 250 ms, where the reads go out at
//! 0, 10, 30, 70, 150 and 310 ms, then every 250 ms.
//!
//! A busy job service still sets the wait ([`Busy::pause_or`](super::busy::Busy::pause_or)):
//! the steps are only the wait of a service that answers.

use std::time::Duration;

/// The wait before the second read of a job that has not started.
pub(crate) const FIRST_STEP: Duration = Duration::from_millis(10);

/// The waits between the reads of one job.
#[derive(Debug)]
pub(crate) struct Pace {
    /// The wait after the next read that finds the job not started.
    step: Duration,
    /// The longest wait, and the wait once the job has run.
    poll: Duration,
}

impl Pace {
    /// The waits of a job whose reads are at most `poll` apart.
    pub(crate) fn new(poll: Duration) -> Self {
        Self {
            step: FIRST_STEP.min(poll),
            poll,
        }
    }

    /// The wait after a read that found the job queued, placed or starting: this step, and
    /// the next one doubles, up to the poll.
    pub(crate) fn not_started(&mut self) -> Duration {
        let wait = self.step;
        self.step = self.step.saturating_mul(2).min(self.poll);
        wait
    }

    /// The wait after a read that found the job running: the poll, and the job is read at the
    /// poll from then on, even if a later read finds it queued again.
    pub(crate) fn running(&mut self) -> Duration {
        self.step = self.poll;
        self.poll
    }
}

#[cfg(test)]
#[path = "pace_tests.rs"]
mod tests;
