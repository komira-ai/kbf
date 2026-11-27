//! The wait before an operation's next attempt, after a run that ended for a reason of the farm.
//!
//! A run that ends for a reason of the farm's and is run again
//! ([`Settled::retried`](super::outcome::Settled::retried): `infra`, `lost`, `out-of-memory`,
//! `input-failed`, `list-refused`) does not have its next job submitted at once. The operation
//! waits first: [`FIRST_POLLS`] polls ([`Config::poll`](super::Config::poll)) before its first
//! rerun, twice the last wait before each one after, at most [`CAP_POLLS`] polls; and each wait
//! is lengthened by up to half again by jitter, so that the operations one fault ended together are not submitted again together. A fault every host
//! shares (a release whose agents refuse every start, a store that stalls) so ends an operation
//! after seconds, not within one poll, and the job service is not sent every operation's next
//! job, with its tree writes, at the moment the last one failed. At the default poll of 250 ms
//! the two reruns of [`RESUBMITS`](super::outcome::RESUBMITS) wait 2 s and 4 s (up to 3 s and 6 s
//! with jitter), and the longest wait is 20 s (30 s with jitter), the longest wait of a refused
//! start too ([`REFUSED_WAIT_CAP`](super::refused::REFUSED_WAIT_CAP)). The waits are counted in
//! polls, as a busy service's and a refused start's are, so an executor that polls faster waits
//! less.
//!
//! The waits stay inside the action's deadline: its timeout, or the executor's default timeout
//! when it names none, before its run was prepared, or for an operation taken up from the state
//! store; counted from when the operation was opened, as a refused start's is
//! ([`super::refused`]). A rerun whose wait would end at or after the deadline is not submitted:
//! the callers are answered the run's own status, its message saying the deadline left no time
//! for another attempt ([`past_deadline`]). A caller's cancel ends the wait. The reruns are
//! counted by the operation's task from zero: an operation taken up from the state store waits
//! [`FIRST_POLLS`] polls before its first rerun again.

use std::time::Duration;

use kbf_proto::google::rpc;
use tokio::time::Instant;

/// The wait before an operation's first rerun, without jitter, in polls: 2 s at the default poll.
pub(crate) const FIRST_POLLS: u32 = 8;

/// The longest wait before a rerun, without jitter, in polls (20 s at the default poll): up to
/// half again with it.
pub(crate) const CAP_POLLS: u32 = 80;

/// The wait before the operation's rerun after `reruns` others (0 for its first), at `poll`:
/// [`FIRST_POLLS`] polls doubled `reruns` times, at most [`CAP_POLLS`] polls, lengthened by
/// `unit` (a number in `[0, 1)`; any other is read as 0) times half of it. A wait too long for a
/// `Duration` is the longest one, unjittered.
pub(crate) fn wait(poll: Duration, reruns: u32, unit: f64) -> Duration {
    let doubled = 1_u32.checked_shl(reruns).unwrap_or(u32::MAX);
    let polls = FIRST_POLLS.saturating_mul(doubled).min(CAP_POLLS);
    let base = poll.saturating_mul(polls);
    let unit = if (0.0..1.0).contains(&unit) {
        unit
    } else {
        0.0
    };
    base.checked_add(base.mul_f64(unit / 2.0)).unwrap_or(base)
}

/// The wait before the operation's rerun after `reruns` others at `poll` ([`wait`], jittered by
/// `unit`), when it ends before `deadline` counted from `now`; `None` when it would end at or
/// after it, and the operation is answered instead.
pub(crate) fn before(
    (poll, reruns, unit): (Duration, u32, f64),
    now: Instant,
    deadline: Instant,
) -> Option<Duration> {
    let wait = wait(poll, reruns, unit);
    now.checked_add(wait)
        .is_some_and(|end| end < deadline)
        .then_some(wait)
}

/// `status`, the answer of a run that would have been run again but for the action's deadline,
/// `timeout` from when it was asked for, with its message saying so.
pub(crate) fn past_deadline(status: &mut rpc::Status, timeout: Duration) {
    status.message = format!(
        "{}; the action's deadline ({}s from when it was asked for) left no time for another \
         attempt",
        status.message,
        timeout.as_secs()
    );
}

#[cfg(test)]
#[path = "rerun_tests.rs"]
mod tests;
