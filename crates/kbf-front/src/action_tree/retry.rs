//! The retry of a create-only write to `cas/l/<sha256>`: what a tree's write does when the
//! store fails it in a way that may pass (a restart, a stall, a dead connection).
//!
//! Every write of a tree is create-only and content-bound (the same key always gets the same
//! bytes), so repeating one is safe: an object an earlier attempt did store is `AlreadyExists`,
//! which the write takes as done once the object's length is the bytes' ([`super::place`]: a
//! write that finds an object of another length under its key, such as one of 0 bytes that a
//! store kept of a cut write, deletes it and writes the object again). The body is made again
//! for each attempt: a blob's is read from its source again, a manifest's, a tree list's or a
//! sidecar's is in hand.
//!
//! The retry is the pack store's ([`crate::store_retry`], run by [`Backoff`]): a request that
//! got no answer, a 503 (`SlowDown` among them), a 500, 502 or 504 and a 400 `RequestTimeout`
//! are made again after jittered waits that grow from the policy's first wait to its longest,
//! until the budget ([`PutRetry::policy`], `--task-put-retry-secs`, [`DEFAULT_TOTAL`] by
//! default), counted from the write's first attempt, would pass before the next attempt began;
//! then the store's last answer is the failure, its message saying how many attempts were made
//! and over how long. A budget of zero retries nothing. The retries are counted with the packs'
//! once the retry is [`PutRetry::counted_in`] the packs' waits, as writes. Every other answer (a
//! 403, a body that failed its digest check) is reported on the first attempt.
//!
//! The waits a write of the tree's manifests, root or list makes are added to that write's tally
//! ([`super::waits`]), as its `store_503` waits.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_objstore::{ObjectKey, ObjectStoreError};
use tokio::time::Instant;

use super::NoTree;
use super::waits::waited_to_retry;
use crate::start_retry::RetryClock;
use crate::store_metrics::Op;
use crate::store_retry::RetryPolicy;
use crate::store_wait::{Backoff, Next, StoreWaits};

/// The default of `--task-put-retry-secs`: the budget of one write of a tree.
pub const DEFAULT_TOTAL: Duration = Duration::from_secs(60);

/// The wait before the first retry, by default ([`RetryPolicy::DEFAULT`]).
pub const FIRST_WAIT: Duration = RetryPolicy::DEFAULT.first;

/// The longest wait before one retry, by default ([`RetryPolicy::DEFAULT`]).
pub const LONGEST_WAIT: Duration = RetryPolicy::DEFAULT.longest;

/// How a wait is taken: the tokio timer in a process, a recording fake in a test. The retries of
/// the pack store's calls read a [`RetryClock`] instead; this paces other waits (the L2
/// backfill's).
pub trait Sleeper: Send + Sync {
    /// Completes after `wait`.
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()>;
}

/// How a tree's writes are repeated. Cheap to clone.
#[derive(Clone, Debug)]
pub struct PutRetry {
    backoff: Backoff,
}

impl PutRetry {
    /// A retry as `policy` says, waiting on the tokio timer, and counted nowhere until
    /// [`PutRetry::counted_in`].
    #[must_use]
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            backoff: Backoff::new(policy, Arc::default()),
        }
    }

    /// [`PutRetry::new`] reading and waiting on `clock` (a test's virtual clock).
    #[must_use]
    pub fn with_clock(policy: RetryPolicy, clock: Arc<dyn RetryClock>) -> Self {
        Self {
            backoff: Backoff::with(policy, clock, Arc::default()),
        }
    }

    /// This retry, its retries counted in `waits` (the packs' [`Packs::store_waits`](crate::Packs::store_waits)),
    /// as writes.
    #[must_use]
    pub fn counted_in(self, waits: Arc<StoreWaits>) -> Self {
        Self {
            backoff: self.backoff.counted_in(waits),
        }
    }

    /// The policy of the retry of one write.
    #[must_use]
    pub const fn policy(&self) -> RetryPolicy {
        self.backoff.policy()
    }
}

impl Default for PutRetry {
    fn default() -> Self {
        Self::new(RetryPolicy::DEFAULT.with_budget(DEFAULT_TOTAL))
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

/// Runs `attempt` (one create-only write of the blob `key`, which checks what the key holds
/// before it writes) until it succeeds, fails in a way that is not retried, or no retry fits in
/// the budget.
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

/// Runs `attempt` until it succeeds, fails in a way that is not retried, or no retry fits in
/// the budget. The failure is as the attempt gave it: the store's last answer, or the
/// [`Failed::Other`] of an attempt.
///
/// # Errors
/// The last attempt's [`Failed`].
pub(super) async fn repeating<E, F, Fut>(retry: &PutRetry, attempt: F) -> Result<(), Failed<E>>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<(), Failed<E>>>,
{
    let mut waits = retry.backoff.call(Op::Put);
    loop {
        let error = match attempt().await {
            Ok(()) => {
                waits.answered();
                return Ok(());
            }
            Err(Failed::Other(failure)) => return Err(Failed::Other(failure)),
            Err(Failed::Store(error)) => error,
        };
        let asked = Instant::now();
        let next = waits.after(&error).await;
        waited_to_retry(asked.elapsed());
        if next == Next::Again {
            continue;
        }
        return Err(Failed::Store(waits.exhausted(error)));
    }
}
