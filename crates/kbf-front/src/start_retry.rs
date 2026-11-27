//! The retry of the pack-store calls a frontend makes before it serves: the check that the
//! store refuses to replace an object ([`Packs::check_create_only`](crate::Packs::check_create_only))
//! and the writes of the kbf base tree ([`crate::base_publish::publish`]).
//!
//! A frontend is often started while its store is itself being restarted (a release rolled to
//! every host at once), and a store that is restarting answers 503 `SlowDown` or refuses the
//! connection for a while. Without a retry, one such answer ends the start, and an update of
//! the frontend is rolled back for it. [`StartRetry`] makes each of those calls again on the
//! answers that say "not now", for a bounded time, and gives up at the bound:
//!
//! - **What is retried** ([`retriable`]): a 503 (`SlowDown` among them), a 500, 502 or 504, a
//!   400 `RequestTimeout` (the store had no byte of the body for its idle timeout), and a
//!   request that got no answer ([`ObjectStoreError::Transport`]: a refused or reset
//!   connection, a request that timed out). Every other answer (a 403, a body that failed its
//!   digest check, an object of another length) is the call's answer at once, as before.
//! - **The waits.** The first is about [`FIRST_WAIT`] and each one after is about twice the
//!   one before, up to [`LONGEST_WAIT`], each jittered to between half and all of it, so that
//!   the frontends a restart stopped together do not come back together.
//! - **The bound.** A call is given up when the bound has passed since its first attempt
//!   began: a failed call whose next wait would reach the bound waits until the bound and is
//!   given up there, and an attempt still unanswered when the bound passes is dropped (every
//!   call retried here is create-only, so an attempt dropped half-way leaves nothing another
//!   attempt trips on but an object it wrote whole). The bound counts the time inside the
//!   attempts too, the waits of the packs' store for its 503s
//!   ([`crate::store_wait`]) among them, so a call never takes longer than the bound. A bound
//!   of zero retries nothing: the first answer is the call's answer, as before the bound.
//! - **What is logged.** One warning for each retry, with the call, the attempt, the wait and
//!   the answer it is a retry of; one line when a call that was retried succeeds, saying how
//!   long it took; and a call given up says in its error how long it was tried and how many
//!   times ([`GaveUp`]).

use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, Either, select};
use kbf_objstore::ObjectStoreError;

use crate::action_tree::{FIRST_WAIT, LONGEST_WAIT};
use crate::store_wait::{Jitter, no_jitter, spread};

/// The default bound of a start-time call to the pack store.
pub const DEFAULT_BOUND: Duration = Duration::from_secs(120);

/// The time a [`StartRetry`] reads and waits on: the tokio timer in a frontend, a virtual clock
/// in a test.
pub trait RetryClock: Send + Sync {
    /// The time now, from an origin of the clock's own.
    fn now(&self) -> Duration;

    /// Completes after `wait`.
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()>;
}

/// The [`RetryClock`] of a running frontend.
struct TokioClock {
    origin: tokio::time::Instant,
}

impl RetryClock for TokioClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(wait))
    }
}

/// How the start-time calls to the pack store are retried, as the module says. Cheap to clone.
#[derive(Clone)]
pub struct StartRetry {
    bound: Duration,
    clock: Arc<dyn RetryClock>,
    jitter: Jitter,
}

impl fmt::Debug for StartRetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartRetry")
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

impl StartRetry {
    /// Retries within `bound`, waiting on the tokio timer, jittered.
    #[must_use]
    pub fn new(bound: Duration) -> Self {
        let clock = Arc::new(TokioClock {
            origin: tokio::time::Instant::now(),
        });
        Self {
            bound,
            clock,
            jitter: spread(),
        }
    }

    /// [`StartRetry::new`] reading and waiting on `clock`, not jittered (a test records the
    /// nominal waits).
    #[must_use]
    pub fn with_clock(bound: Duration, clock: Arc<dyn RetryClock>) -> Self {
        Self {
            bound,
            clock,
            jitter: no_jitter(),
        }
    }

    /// The bound of one call.
    #[must_use]
    pub const fn bound(&self) -> Duration {
        self.bound
    }

    /// Makes the call `what` by `attempt` (given the number of the attempt, 1 for the first)
    /// until it succeeds, fails with an error `retriable` does not take, or the bound passes.
    ///
    /// # Errors
    /// [`Ended::Refused`] with an answer that is not retried (or, with a bound of zero, the
    /// first answer); [`Ended::GaveUp`] when the bound passed with the call still failing.
    pub async fn run<T, E, R, F, Fut>(
        &self,
        what: &str,
        retriable: R,
        mut attempt: F,
    ) -> Result<T, Ended<E>>
    where
        E: fmt::Display,
        R: Fn(&E) -> bool,
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if self.bound.is_zero() {
            return attempt(1).await.map_err(Ended::Refused);
        }
        let started = self.clock.now();
        let deadline = started + self.bound;
        let mut nominal = FIRST_WAIT;
        let mut last = None;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let left = deadline.saturating_sub(self.clock.now());
            let answered = match select(pin!(attempt(attempts)), self.clock.sleep(left)).await {
                Either::Left((answer, _)) => Some(answer),
                Either::Right(((), _)) => None,
            };
            let tried = self.clock.now().saturating_sub(started);
            let error = match answered {
                Some(Ok(value)) => {
                    if attempts > 1 {
                        tracing::info!(
                            call = what,
                            attempts,
                            seconds = tried.as_secs_f64(),
                            "the pack store answered a call of the start after it was retried"
                        );
                    }
                    return Ok(value);
                }
                Some(Err(error)) if !retriable(&error) => return Err(Ended::Refused(error)),
                Some(Err(error)) => error,
                None => {
                    return Err(Ended::GaveUp(GaveUp {
                        what: what.to_owned(),
                        last,
                        attempts,
                        tried,
                        unanswered: true,
                    }));
                }
            };
            let left = deadline.saturating_sub(self.clock.now());
            let wait = (self.jitter)(nominal);
            if wait >= left {
                // No attempt would have time to be answered: the call is given up at the bound.
                self.clock.sleep(left).await;
                return Err(Ended::GaveUp(GaveUp {
                    what: what.to_owned(),
                    last: Some(error),
                    attempts,
                    tried: self.clock.now().saturating_sub(started),
                    unanswered: false,
                }));
            }
            tracing::warn!(
                call = what,
                attempt = attempts,
                wait_seconds = wait.as_secs_f64(),
                left_seconds = left.as_secs_f64(),
                reason = %error,
                "a call of the start to the pack store failed; trying it again"
            );
            self.clock.sleep(wait).await;
            nominal = (nominal * 2).min(LONGEST_WAIT);
            last = Some(error);
        }
    }
}

/// Whether the store's answer `error` is one a start-time call is made again for, as the module
/// says.
#[must_use]
pub fn retriable(error: &ObjectStoreError) -> bool {
    match error {
        ObjectStoreError::Service {
            status: 500 | 502 | 503 | 504,
            ..
        }
        | ObjectStoreError::Transport(_) => true,
        ObjectStoreError::Service {
            status: 400, code, ..
        } => code == "RequestTimeout",
        _ => false,
    }
}

/// How a call [`StartRetry::run`] made ended without success.
#[derive(Debug)]
pub enum Ended<E> {
    /// An answer that is not retried: the call's answer at once.
    Refused(E),
    /// The bound passed with the call still failing.
    GaveUp(GaveUp<E>),
}

impl<E: fmt::Display> fmt::Display for Ended<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(error) => fmt::Display::fmt(error, f),
            Self::GaveUp(gave_up) => fmt::Display::fmt(gave_up, f),
        }
    }
}

/// A call given up at the bound: what it was, the last answer and how long it was tried.
#[derive(Debug)]
pub struct GaveUp<E> {
    /// The call.
    pub what: String,
    /// The last answer of the store; `None` when the first attempt was still unanswered at the
    /// bound.
    pub last: Option<E>,
    /// How many attempts were made, the dropped one included.
    pub attempts: u32,
    /// How long the call was tried, from its first attempt to the end.
    pub tried: Duration,
    /// Whether the last attempt was dropped unanswered at the bound.
    pub unanswered: bool,
}

impl<E: fmt::Display> fmt::Display for GaveUp<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.tried.as_secs_f64();
        let attempts = self.attempts;
        match (&self.last, self.unanswered) {
            (Some(last), false) => write!(
                f,
                "{last} (still failing after {attempts} attempts over {seconds:.1} s)"
            ),
            (Some(last), true) => write!(
                f,
                "{last} (still failing after {attempts} attempts over {seconds:.1} s, the last \
                 of them unanswered)"
            ),
            (None, _) => write!(
                f,
                "{}: the pack store did not answer within {seconds:.1} s",
                self.what
            ),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for GaveUp<E> {}

#[cfg(test)]
#[path = "start_retry_tests.rs"]
mod tests;
