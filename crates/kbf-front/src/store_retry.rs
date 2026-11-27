//! The one retry policy of the frontend's calls to the pack store after it has started:
//! [`RetryPolicy`], its numbers, and [`Cause`], which answers of the store it makes a call
//! again for. The engine that applies it is [`crate::store_wait`] ([`Backoff`](crate::store_wait::Backoff),
//! under every read, length, delete and listing of the packs, and every whole write); the
//! writes whose body is read from a source again (an action tree's blobs, manifests, list and
//! sidecar, [`PutRetry`](crate::action_tree::PutRetry)) run the same engine around their
//! attempts. Each call has its own budget: a restart of the store, a stall or a reset
//! connection costs the call a wait, not a failure, and never more than the budget.
//!
//! - **What is retried** ([`Cause::of`]): a request that got no answer
//!   ([`ObjectStoreError::Transport`]: a refused or reset connection, a dead pooled connection,
//!   a request or a body the store client cut for making no progress), a 503 (`SlowDown`
//!   among them), a 500, 502 or 504, and a 400 `RequestTimeout` (the store had no byte of a
//!   body for its idle timeout). Every other answer (a 403, a 404, a body that failed its digest
//!   check, an object of another length) is the call's answer at once.
//! - **The waits.** The first is [`RetryPolicy::first`]; each one after is
//!   [`RetryPolicy::growth`] times the one before, up to [`RetryPolicy::longest`]. Each is
//!   jittered: a wait of a nominal `w` lasts between `w` less [`RetryPolicy::jitter_percent`] of
//!   it and `w`, so the callers one restart stopped together do not come back together.
//! - **The budget** ([`RetryPolicy::budget`]) counts from the call's first attempt, the time
//!   inside the attempts included. A retry whose wait would end past the budget is not made: the
//!   last answer is the call's, saying how many attempts it made and over how long. An attempt in
//!   flight is never cut by the budget; the store client's own timeouts end it (no byte of a
//!   response for its request timeout, 60 s in a frontend), so a call lasts at most its budget
//!   and one attempt.
//!
//! The defaults ([`RetryPolicy::DEFAULT`]) against the other side's patience:
//!
//! - **The budget, 120 s**, rides out a tierstore restart (its drain serves for 10 s, closes
//!   its idle connections and hands its sockets on, so pooled connections reset), a store that
//!   fails over its metadata (503 for up to about 30 s), a stall cut by the client at 60 s and
//!   the attempt after it, and a store that answers 503 for a minute. A store that answers
//!   (each attempt ends quickly) ends the call's retries inside the frontend listener's bound
//!   on a dead connection (`--http2-keepalive-interval-secs` 30 and
//!   `--http2-keepalive-timeout-secs` 120: 150 s with no byte either way), so a client that
//!   waits on such a call has its answer before the listener would give up on it; a call whose
//!   last attempt stalls may run past the budget by that attempt, up to the client's 60 s.
//! - **The longest wait, 8 s**, is well under tierstore's idle bounds: it closes a connection
//!   that sends no request for its header timeout (60 s) and a body that sends no byte for its
//!   body idle timeout (30 s). A wait holds no request open: it is taken between attempts, and
//!   the next attempt's connection is a new one or one the client's pool has kept for less than
//!   its 15 s idle bound.
//! - **The first wait, 0.5 s, doubling**, makes a reset pooled connection (the common case
//!   while a store restarts) cost half a second, and reaches the longest wait after four waits.
//! - **The jitter, 50 %**, spreads each wait over its second half.

use std::time::Duration;

use kbf_objstore::ObjectStoreError;

/// The numbers of the retry of one call to the pack store, as the module says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The wait before the first retry, before jitter.
    pub first: Duration,
    /// How many times the wait before a retry is the one before it, before jitter: 1 keeps
    /// every wait at [`RetryPolicy::first`]. 0 is taken as 1.
    pub growth: u32,
    /// The longest wait before one retry, before jitter: the growth stops here.
    pub longest: Duration,
    /// How much of a wait the jitter may take off, in percent of it: a nominal wait `w` lasts
    /// between `w * (100 - jitter_percent) / 100` and `w`. 0 waits exactly `w`; more than 100
    /// is taken as 100.
    pub jitter_percent: u8,
    /// How long one call may be tried, from its first attempt: no retry starts that would
    /// begin past it. Zero retries nothing.
    pub budget: Duration,
}

impl RetryPolicy {
    /// The defaults of the frontend's flags (`--pack-store-retry-*`), as the module says.
    pub const DEFAULT: Self = Self {
        first: Duration::from_millis(500),
        growth: 2,
        longest: Duration::from_secs(8),
        jitter_percent: 50,
        budget: Duration::from_secs(120),
    };

    /// This policy with `budget` for each call.
    #[must_use]
    pub const fn with_budget(self, budget: Duration) -> Self {
        Self { budget, ..self }
    }

    /// The nominal wait after one of `nominal`: grown, and no longer than the longest.
    #[must_use]
    pub fn grown(&self, nominal: Duration) -> Duration {
        nominal.saturating_mul(self.growth.max(1)).min(self.longest)
    }

    /// The wait taken for a nominal wait of `nominal`, given a random draw `z`: between
    /// `nominal` less the jitter's share of it and `nominal`.
    #[must_use]
    pub fn jittered(&self, nominal: Duration, z: u64) -> Duration {
        let percent = u128::from(self.jitter_percent.min(100));
        let most_off = nominal.as_nanos().saturating_mul(percent) / 100;
        let span = u64::try_from(most_off).unwrap_or(u64::MAX);
        let off = Duration::from_nanos(z.checked_rem(span.saturating_add(1)).unwrap_or(0));
        nominal.saturating_sub(off)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a call to the pack store was made again: the label `cause` of
/// `kbf_pack_store_retries_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The request got no answer: a refused or reset connection, a request the client cut.
    Connection,
    /// A 503 `SlowDown`: the store is taking as many requests as it can.
    SlowDown,
    /// Any other 503: the store is not serving for a while (a restart, a failover).
    Unavailable,
    /// A 500, 502 or 504.
    ServerError,
    /// A 400 `RequestTimeout`: the store had no byte of the body for its idle timeout.
    RequestTimeout,
}

impl Cause {
    /// Every cause, in the order the family lists them.
    pub const ALL: [Self; 5] = [
        Self::Connection,
        Self::SlowDown,
        Self::Unavailable,
        Self::ServerError,
        Self::RequestTimeout,
    ];

    /// The cause of the store's answer `error`, if it is one a call is made again for.
    #[must_use]
    pub fn of(error: &ObjectStoreError) -> Option<Self> {
        match error {
            ObjectStoreError::Transport(_) => Some(Self::Connection),
            ObjectStoreError::Service {
                status: 503, code, ..
            } if code == "SlowDown" => Some(Self::SlowDown),
            ObjectStoreError::Service { status: 503, .. } => Some(Self::Unavailable),
            ObjectStoreError::Service {
                status: 500 | 502 | 504,
                ..
            } => Some(Self::ServerError),
            ObjectStoreError::Service {
                status: 400, code, ..
            } if code == "RequestTimeout" => Some(Self::RequestTimeout),
            _ => None,
        }
    }

    /// The label value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::SlowDown => "slow_down",
            Self::Unavailable => "unavailable",
            Self::ServerError => "server_error",
            Self::RequestTimeout => "request_timeout",
        }
    }
}

#[cfg(test)]
#[path = "store_retry_tests.rs"]
mod tests;
