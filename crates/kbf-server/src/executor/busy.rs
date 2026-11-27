//! A job service that is busy or cannot be reached: its calls wait, and nothing is decided.
//!
//! A job service that stalls answers late or not at all. A call past
//! [`Config::call_timeout`](super::Config::call_timeout) is dropped, and HTTP/2 resets its
//! stream. A server that is sent many resets on one connection closes it with a GOAWAY
//! (`ENHANCE_YOUR_CALM`), which every call in flight on it reads as `RESOURCE_EXHAUSTED`; the
//! client's channel dials a new connection for the next call by itself. None of that says
//! anything about a job, so a busy answer ([`busy`]) never settles a run and never spends an
//! attempt: the call is sent again (a submit with the same spec under the same id, which the
//! service takes once), after a wait. So is a status that says the service will not serve this
//! frontend now (`UNAUTHENTICATED`, `PERMISSION_DENIED`, `UNIMPLEMENTED`): a token or a revision
//! the whole farm shares, never the job's, and a run ended for it would be a fault the next
//! attempt meets again.
//!
//! The wait is the executor's, not one operation's ([`Busy`]): every operation's task reads the
//! same level, so a thousand operations that poll their jobs slow down together, and the
//! service that stalled is not sent a thousand calls each poll. The first busy answer makes the
//! wait one poll; each busy answer after the current wait has half passed doubles it, up to
//! [`MAX_POLLS`] polls (10 s at the default poll); an answer of the service ends it once a whole wait has passed with no busy
//! answer of any call, and the operations read at their own waits again (the poll, or the
//! shorter steps of a job that has not started: [`super::pace`]). A service that answers some
//! calls and not others (reads of jobs, but not of results) so stays busy, and its waits go on
//! doubling, rather than ending and starting again at every answer.
//! Each wait is jittered down by up to half ([`crate::join::jittered`]), so operations that wait
//! together do not call together. The executor logs once when the service turns busy, at WARN,
//! and once when it answers again, and not for each busy answer between.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use kbf_job_client::{Error, Unsent};
use tokio::time::Instant;
use tonic::Code;

use crate::join::jittered;

/// The longest wait between calls while the job service is busy, in polls: 10 s at the default
/// poll of 250 ms.
pub(crate) const MAX_POLLS: u32 = 40;

/// Whether `error` says the job service is busy or cannot be reached, and says nothing of the
/// call: no answer came (the call's deadline passed, the connection broke or reset), no
/// connection could be made, or the service answered a status of its own load or of its
/// stream (`RESOURCE_EXHAUSTED`, which is also how a GOAWAY `ENHANCE_YOUR_CALM` reads,
/// `UNAVAILABLE`, `DEADLINE_EXCEEDED`, `ABORTED`, `CANCELLED`, `UNKNOWN`, `INTERNAL`), or one that
/// says it will not serve this frontend at all now (`UNAUTHENTICATED`, `PERMISSION_DENIED`: a
/// token revoked or not yet accepted; `UNIMPLEMENTED`: a service of another revision behind the
/// address), which says nothing of the call either: the readiness probe reads those as a service
/// that cannot be reached too ([`answered`](super::api::answered)). A refusal, a status about the
/// request (`INVALID_ARGUMENT`, `NOT_FOUND`, `FAILED_PRECONDITION`, ...), an answer that cannot be
/// decoded, and a request that could never be sent are not.
pub(crate) fn busy(error: &Error) -> bool {
    match error {
        Error::Transport(_) | Error::Unreachable(Unsent::Connect(_)) => true,
        Error::Status { code, .. } => matches!(
            code,
            Code::ResourceExhausted
                | Code::Unavailable
                | Code::DeadlineExceeded
                | Code::Aborted
                | Code::Cancelled
                | Code::Unknown
                | Code::Internal
                | Code::Unauthenticated
                | Code::PermissionDenied
                | Code::Unimplemented
        ),
        Error::Unreachable(Unsent::BadId(_) | Unsent::TooLarge { .. })
        | Error::Decode(_)
        | Error::Refused(_)
        | Error::TooMany(_)
        | Error::Stuck(_) => false,
    }
}

/// The job service's busy level, shared by every operation of the executor.
pub(crate) struct Busy {
    /// The wait at the first level: the poll.
    base: Duration,
    /// A number in `[0, 1)` for each wait's jitter.
    unit: fn() -> f64,
    state: Mutex<State>,
}

/// Where the level is.
#[derive(Default)]
struct State {
    /// 0 while the service answers; then 1, 2, ...: the wait is `base * 2^(level - 1)`.
    level: u32,
    /// When the level last went up.
    raised: Option<Instant>,
    /// When the last busy answer came.
    last: Option<Instant>,
    /// When the service turned busy.
    since: Option<Instant>,
    /// The busy answers since then.
    calls: u64,
}

/// What one answer of the service was to the level, and what was logged of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// An answer while the service answers, or one within a wait of a busy answer (the level
    /// stays): nothing logged.
    Answered,
    /// The first answer a whole wait after the last busy one: logged at INFO, and the level
    /// ends.
    AnsweredAgain,
    /// The first busy answer: logged at WARN.
    TurnedBusy,
    /// A busy answer after another: logged at DEBUG only.
    StillBusy,
}

impl Seen {
    /// Whether the answer was a busy one ([`busy`]).
    pub(crate) const fn busy(self) -> bool {
        matches!(self, Self::TurnedBusy | Self::StillBusy)
    }
}

/// The highest level counted: past it the wait is [`MAX_POLLS`] polls in any case.
const TOP_LEVEL: u32 = 32;

impl Busy {
    /// A level that waits `base` (the poll) when the service is not busy, jittered by `unit`.
    pub(crate) fn new(base: Duration, unit: fn() -> f64) -> Self {
        Self {
            base,
            unit,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The wait at `level`: `base` doubling, at most [`MAX_POLLS`] of it.
    fn step(&self, level: u32) -> Duration {
        let doubled = 1_u32
            .checked_shl(level.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.base.saturating_mul(doubled.min(MAX_POLLS))
    }

    /// Takes in what a call of `what` was answered: a busy error ([`busy`]) raises the level,
    /// and anything else ends it once a whole wait has passed since the last busy one. The caller logs the errors that were not busy
    /// ([`Seen::busy`]); this logs the turns.
    pub(crate) fn saw<T>(&self, what: &str, answer: &Result<T, Error>) -> Seen {
        match answer {
            Err(error) if busy(error) => self.failed(what, error, Instant::now()),
            _ => self.answered(Instant::now()),
        }
    }

    fn failed(&self, what: &str, error: &Error, now: Instant) -> Seen {
        let mut state = self.state();
        state.calls += 1;
        state.last = Some(now);
        let Some(raised) = state.raised else {
            state.level = 1;
            state.raised = Some(now);
            state.since = Some(now);
            drop(state);
            let most = self.step(TOP_LEVEL);
            tracing::warn!(%error, "the job service is busy or cannot be reached ({what} failed): its calls wait and are sent again, backing off up to {most:?}; no run ends for it");
            return Seen::TurnedBusy;
        };
        // Every operation sees the same busy answer: the level goes up once a wait.
        if now.saturating_duration_since(raised) >= self.step(state.level) / 2 {
            state.level = (state.level + 1).min(TOP_LEVEL);
            state.raised = Some(now);
        }
        drop(state);
        tracing::debug!(%error, "{what} failed: the job service is still busy");
        Seen::StillBusy
    }

    fn answered(&self, now: Instant) -> Seen {
        let mut state = self.state();
        let quiet = state
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= self.step(state.level));
        if state.level == 0 || !quiet {
            return Seen::Answered;
        }
        let since = state.since.map(|since| since.elapsed());
        let calls = state.calls;
        *state = State::default();
        drop(state);
        tracing::info!(
            busy_for = ?since,
            busy_calls = calls,
            "the job service answers again"
        );
        Seen::AnsweredAgain
    }

    /// How long an operation waits before its next call to the service: the poll while it
    /// answers, else the level's wait, jittered.
    pub(crate) fn pause(&self) -> Duration {
        self.pause_or(self.base)
    }

    /// [`Busy::pause`] with `quiet` as the wait while the service answers: the read of a job
    /// that has not started waits less than the poll ([`super::pace`]), and as long as any
    /// other call while the service is busy.
    pub(crate) fn pause_or(&self, quiet: Duration) -> Duration {
        let level = self.state().level;
        if level == 0 {
            return quiet;
        }
        jittered(self.step(level), (self.unit)())
    }
}

#[cfg(test)]
#[path = "busy_tests.rs"]
mod tests;
