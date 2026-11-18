//! The retry of a create-only write to `cas/l/<sha256>`: what a tree's write does when tierstore
//! answers "retry later" or a pooled connection turns out to be dead.
//!
//! Every write of a tree is create-only and content-bound (the same key always gets the same
//! bytes), so repeating one is safe: an object an earlier attempt did store is `AlreadyExists`,
//! which the write counts as done. Three answers are repeated, and no other:
//!
//! - **A connection-level failure** ([`ObjectStoreError::Transport`]: a pooled connection the
//!   store had closed, "connection closed before message completed", "Broken pipe"): once, at
//!   once, on the connection the client makes next. A second failure is the failure.
//! - **A 503** ([`ObjectStoreError::Service`] with status 503: the store is not taking writes
//!   for a while, typically 10 to 40 s while it registers): after a wait that starts at
//!   [`FIRST_WAIT`] and doubles up to [`LONGEST_WAIT`], until the waits add up to
//!   [`PutRetry::total`] (`--task-put-retry-secs`, [`DEFAULT_TOTAL`] by default); the wait that
//!   would pass the total is cut to end there, and when the waits have added up to it, the 503
//!   is the failure. A total of zero never retries a 503.
//! - **A 503 `SlowDown`** (the store is taking as many uploads as it can: back-pressure, not a
//!   fault): with the same doubling waits, for as long as the store answers it; its waits do
//!   not count toward the total. What ends it is the caller: a preparation that runs out of its
//!   limit drops the write, and with it the retry.
//!
//! Every other answer (a 403, a 500, a body that failed its digest check) is reported on the
//! first attempt. A write whose body is read from a source is made again by reading the source
//! again: the attempt is a function that builds its own body.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_objstore::{ObjectKey, ObjectStoreError};

use super::NoTree;

/// The default of `--task-put-retry-secs`: how long the waits before the retries of one write
/// of a tree may add up to.
pub const DEFAULT_TOTAL: Duration = Duration::from_secs(60);

/// The wait before the first retry of a 503.
pub const FIRST_WAIT: Duration = Duration::from_millis(500);

/// The longest wait before one retry of a 503: the wait doubles up to this.
pub const LONGEST_WAIT: Duration = Duration::from_secs(8);

/// How the retry waits: the tokio timer in a frontend, a recording fake in a test.
pub trait Sleeper: Send + Sync {
    /// Completes after `wait`.
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()>;
}

/// The [`Sleeper`] of a running frontend.
struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(tokio::time::sleep(wait))
    }
}

/// How a tree's writes are repeated. Cheap to clone.
#[derive(Clone)]
pub struct PutRetry {
    total: Duration,
    sleeper: Arc<dyn Sleeper>,
}

impl PutRetry {
    /// A retry whose waits for 503s add up to at most `total`, waiting on the tokio timer.
    #[must_use]
    pub fn new(total: Duration) -> Self {
        Self::with_sleeper(total, Arc::new(TokioSleeper))
    }

    /// [`PutRetry::new`] waiting with `sleeper` (a test records the waits instead).
    #[must_use]
    pub fn with_sleeper(total: Duration, sleeper: Arc<dyn Sleeper>) -> Self {
        Self { total, sleeper }
    }

    /// The most the waits before the retries of one write add up to.
    #[must_use]
    pub const fn total(&self) -> Duration {
        self.total
    }
}

impl Default for PutRetry {
    fn default() -> Self {
        Self::new(DEFAULT_TOTAL)
    }
}

/// Why one attempt of a write failed. A write that can fail only at the store (a manifest's,
/// whose bytes are in hand) takes `E` as [`std::convert::Infallible`].
pub(super) enum Failed<E = NoTree> {
    /// The store's answer, which may be repeated.
    Store(ObjectStoreError),
    /// A failure that is not the store's (the blob's read, its digest): final.
    Other(E),
}

/// Whether `error` is the store's back-pressure: a 503 `SlowDown`.
fn is_slow_down(error: &ObjectStoreError) -> bool {
    matches!(error, ObjectStoreError::Service { status: 503, code, .. } if code == "SlowDown")
}

/// Whether `error` is a 503 of the store.
fn is_unavailable(error: &ObjectStoreError) -> bool {
    matches!(error, ObjectStoreError::Service { status: 503, .. })
}

/// Runs `attempt` (one create-only write of `key`), and again after a connection-level failure
/// (once), a 503 (within the total) or a 503 `SlowDown` (for as long as the store answers it).
///
/// # Errors
/// [`NoTree::Put`] with the store's last answer, or the [`Failed::Other`] of an attempt.
pub(super) async fn retrying<F, Fut>(
    retry: &PutRetry,
    key: &ObjectKey,
    attempt: F,
) -> Result<(), NoTree>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<(), Failed>>,
{
    repeating(retry, attempt)
        .await
        .map_err(|failed| match failed {
            Failed::Store(error) => NoTree::Put {
                key: key.to_string(),
                why: error.to_string(),
            },
            Failed::Other(failure) => failure,
        })
}

/// [`retrying`], the failure as the attempt gave it: the store's last answer, or the
/// [`Failed::Other`] of an attempt.
///
/// # Errors
/// The last attempt's [`Failed`].
pub(super) async fn repeating<E, F, Fut>(retry: &PutRetry, attempt: F) -> Result<(), Failed<E>>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<(), Failed<E>>>,
{
    let mut reconnected = false;
    let mut waited = Duration::ZERO;
    let mut next = FIRST_WAIT;
    loop {
        let error = match attempt().await {
            Ok(()) => return Ok(()),
            Err(Failed::Other(failure)) => return Err(Failed::Other(failure)),
            Err(Failed::Store(ObjectStoreError::AlreadyExists(_))) => return Ok(()),
            Err(Failed::Store(error)) => error,
        };
        if matches!(error, ObjectStoreError::Transport(_)) && !reconnected {
            reconnected = true;
            continue;
        }
        if is_slow_down(&error) {
            retry.sleeper.sleep(next).await;
            next = (next * 2).min(LONGEST_WAIT);
            continue;
        }
        if is_unavailable(&error) && waited < retry.total {
            let wait = next.min(retry.total - waited);
            retry.sleeper.sleep(wait).await;
            waited += wait;
            next = (next * 2).min(LONGEST_WAIT);
            continue;
        }
        return Err(Failed::Store(error));
    }
}
