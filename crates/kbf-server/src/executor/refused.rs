//! A start a host would not make now, and how long an operation goes on placing its job again.
//!
//! The job service places a job on a host and the host's agent may still refuse to start it:
//! the job does not fit what is free there (`no-capacity`), the host is held while it is drained
//! for an update (`host-held`), or it is tainted (`host-tainted`). The job service itself may
//! also end a job that found no room within its pool's queue wait (`NO_CAPACITY`). None of these
//! says anything of the action: another host, or the same one later, starts the same job. Such
//! a job ends failed to start, and the job service does not place it again, so the executor
//! submits the operation's next job ([`Verdict::Refused`](super::outcome::Verdict::Refused)),
//! spending none of the attempts of [`RESUBMITS`](super::outcome::RESUBMITS).
//!
//! **The bound.** Each refused start waits before the next job is submitted: the executor's poll
//! the first time, twice that after each refusal, at most [`REFUSED_WAIT_CAP`]. The operation
//! places its job again while the next job would be submitted before the action's own deadline
//! (its timeout, or the executor's default timeout when it names none, counted from when the
//! operation was opened), and at most [`REFUSED_STARTS`] times. Past either, its callers are
//! answered `UNAVAILABLE` with a message that names every refusal it was given and how many
//! times ([`Refusals::status`]). An operation taken up from the state store counts its refusals
//! from zero, and takes the default timeout as its action's.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use kbf_proto::google::rpc;
use tokio::time::Instant;
use tonic::Code;

/// The most refused starts an operation places again.
pub(crate) const REFUSED_STARTS: u32 = 100;

/// The longest wait before a refused start is placed again.
pub(crate) const REFUSED_WAIT_CAP: Duration = Duration::from_secs(30);

/// Why a host would not start a job now: not the action's failure, and not for ever.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Refusal {
    /// No room: the job service found none within its pool's queue wait, or the host's agent
    /// found the job does not fit what is free on it.
    NoCapacity,
    /// The host is held: drained for an update, or its update halted.
    HostHeld,
    /// The host is tainted.
    HostTainted,
}

impl Refusal {
    /// The code the job service or the agent gives it.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::NoCapacity => "no-capacity",
            Self::HostHeld => "host-held",
            Self::HostTainted => "host-tainted",
        }
    }
}

/// Why an operation stops placing its refused job again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The next job would be submitted at or after the action's deadline.
    Deadline,
    /// It was placed again [`REFUSED_STARTS`] times.
    Bound,
}

/// The refused starts of one operation, counted by its task from zero.
#[derive(Debug, Default)]
pub(crate) struct Refusals {
    /// How many of each refusal it was given.
    by_code: BTreeMap<Refusal, u32>,
    /// How many it was given in all.
    total: u32,
}

impl Refusals {
    /// How many refused starts were placed again: each spent no attempt.
    pub(crate) const fn placed_again(&self) -> u32 {
        self.total
    }

    /// Counts `refusal`, given at `now`, and says what follows: the wait before the job is
    /// placed again, its first `poll` and doubling after each refusal up to
    /// [`REFUSED_WAIT_CAP`]; or why it is not, when the operation has placed its job again
    /// [`REFUSED_STARTS`] times or the wait would end at or after `deadline`.
    ///
    /// # Errors
    /// Why the operation stops placing its job again.
    pub(crate) fn add(
        &mut self,
        refusal: Refusal,
        poll: Duration,
        now: Instant,
        deadline: Instant,
    ) -> Result<Duration, Stop> {
        let before = self.total;
        *self.by_code.entry(refusal).or_default() += 1;
        if before >= REFUSED_STARTS {
            return Err(Stop::Bound);
        }
        let wait = poll
            .saturating_mul(1_u32 << before.min(16))
            .min(REFUSED_WAIT_CAP);
        if now + wait >= deadline {
            return Err(Stop::Deadline);
        }
        self.total += 1;
        Ok(wait)
    }

    /// What the callers are answered when the operation stopped for `stop`, its action's
    /// timeout being `timeout`: `UNAVAILABLE`, naming every refusal it was given and how many
    /// times, the last among them.
    pub(crate) fn status(&self, stop: Stop, timeout: Duration) -> rpc::Status {
        let given: u32 = self.by_code.values().sum();
        let mut named = String::new();
        for (refusal, count) in &self.by_code {
            let comma = if named.is_empty() { "" } else { ", " };
            let _ = write!(named, "{comma}{} {count}", refusal.code());
        }
        let why = match stop {
            Stop::Deadline => format!(
                "the next placement would come after the action's deadline ({}s from when it \
                 was asked for)",
                timeout.as_secs()
            ),
            Stop::Bound => format!(
                "the job was placed again {REFUSED_STARTS} times, the most an operation places it"
            ),
        };
        rpc::Status {
            code: Code::Unavailable as i32,
            message: format!(
                "the farm's hosts refused to start the job {given} times and {why}: {named} \
                 (not the action's own failure)"
            ),
            details: Vec::new(),
        }
    }
}

#[cfg(test)]
#[path = "refused_tests.rs"]
mod tests;
