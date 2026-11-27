//! The frontend's retry of its calls to the pack store: [`Waiting`], which
//! [`Packs`](crate::Packs) puts in front of its store, so that every call the frontend makes of
//! the packs' store (client uploads and reads, the executor's preparation and ingest, an
//! Execute's input walk, base publication, the janitor's listings and deletes) rides out a store
//! that restarts, stalls or drops a connection, and [`Backoff`], the engine of
//! [`RetryPolicy`](crate::store_retry::RetryPolicy) that it runs, which the action trees' own
//! retry ([`PutRetry`](crate::action_tree::PutRetry)) shares.
//!
//! - **What is retried, the waits and the budget** are the policy's
//!   ([`crate::store_retry`]): a request that got no answer, a 503 (`SlowDown` among them), a
//!   500, 502 or 504 and a 400 `RequestTimeout` are made again after jittered waits that grow
//!   from the first to the longest, until the call's budget, counted from its first attempt,
//!   would pass before the next attempt began; every other answer is the call's at once. The
//!   store's `Retry-After` is not read: the store client gives kbf no header of an error answer.
//! - **The last answer.** A call that is still failing when no retry fits in its budget answers
//!   the store's last answer, its message saying how many attempts were made and over how long
//!   ([`Waits::exhausted`]). A 503 keeps its status and code, and a request that got no answer
//!   stays [`ObjectStoreError::Transport`], so the callers take each as they did: a client's
//!   call is answered UNAVAILABLE, which the client repeats, and an Execute's input walk ends
//!   UNAVAILABLE, never as the action's own failure.
//! - **A store that is down.** Once a call has spent its budget with the store still failing,
//!   the store is taken as down: the first retriable failure of every call after that is its
//!   answer at once, with no wait, until the store answers a call with anything but a retriable
//!   failure, which takes it as up again. So a store down for good costs the budget once, not
//!   once per call: a pack written again under another key after a failed write, the delete of
//!   the failed key, the next of a preparation's reads, are each answered at once. The mark is
//!   shared by every caller that counts its waits in the same [`StoreWaits`] (the packs, the
//!   action trees' retry, the janitor).
//! - **A whole write** ([`ObjectStore::put_new`]: a pack) is made again with its body, which is
//!   in hand. Every write the packs make is create-only, under a key no other write uses (a
//!   pack's sequence key) or one its content names (`cas/l/<sha256>`), so a write whose answer
//!   was lost and is made again is answered `AlreadyExists`, which its caller takes as it must
//!   (the packer writes the pack under its next key; a tree's write checks the object's length).
//! - **A streamed write** ([`ObjectStore::put_stream`]) is made once here: its body cannot be
//!   read twice. Its caller repeats it if it can make the body again (an action tree's write
//!   reads its source again, with the same [`Backoff`]); a client's upload over the packs' bound
//!   cannot, and the store's answer ends it UNAVAILABLE, which the client repeats. Such an upload
//!   asks the store whether it holds the object first ([`ObjectStore::len`]), and that lookup is
//!   retried, so a repeated upload waits out a restart before it is sent. A streamed write that
//!   dies part way stores nothing a reader takes for whole: the store keeps an object only once
//!   it has every byte of the length the request declared, and a write that finds its key held
//!   by an object of another length (what a store that kept a cut write would leave, such as an
//!   object of 0 bytes) deletes it and writes the object again
//!   ([`crate::action_tree`]'s writes), or refuses it ([`crate::Packs`]' large uploads).
//! - **Metrics.** Every retry is recorded in [`StoreWaits`] by the [`Op`] of the call and its
//!   [`Cause`] (`kbf_pack_store_retries_total`), every wait with its length
//!   (`kbf_pack_store_wait_seconds`), and the calls that ended with no retry left in their
//!   budget, or were answered at once while the store was taken as down
//!   (`kbf_pack_store_waits_exhausted_total`).

use std::error::Error;
use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use crate::metrics::{Counter, Histogram, Unit};
use crate::start_retry::{RetryClock, tokio_clock};
use crate::store_metrics::Op;
use crate::store_retry::{Cause, RetryPolicy};

/// The default budget of one call to the pack store: [`RetryPolicy::DEFAULT`]'s.
pub const STORE_WAIT: Duration = RetryPolicy::DEFAULT.budget;

/// What the retries of the calls to the store have been: by [`Op`] and [`Cause`], each retry;
/// by [`Op`], each wait and its length, and the calls that ended with no retry left in their
/// budget; and whether the store is taken as down (as the module says).
#[derive(Debug)]
pub struct StoreWaits {
    seconds: [Histogram; Op::ALL.len()],
    retries: [[Counter; Cause::ALL.len()]; Op::ALL.len()],
    exhausted: [Counter; Op::ALL.len()],
    walks_cut: Counter,
    down: AtomicBool,
}

impl Default for StoreWaits {
    fn default() -> Self {
        Self {
            seconds: std::array::from_fn(|_| Histogram::new(Unit::Seconds)),
            retries: Default::default(),
            exhausted: Default::default(),
            walks_cut: Counter::default(),
            down: AtomicBool::new(false),
        }
    }
}

impl StoreWaits {
    /// The waits of the calls of `op`: one value per wait, its length.
    #[must_use]
    pub fn waits(&self, op: Op) -> &Histogram {
        &self.seconds[op as usize]
    }

    /// The retries of the calls of `op` made for `cause`.
    #[must_use]
    pub fn retries(&self, op: Op, cause: Cause) -> u64 {
        self.retries[op as usize][cause as usize].get()
    }

    /// The calls of `op` that ended with the store still failing and no retry left in their
    /// budget, and those answered at once while the store was taken as down.
    #[must_use]
    pub fn exhausted(&self, op: Op) -> u64 {
        self.exhausted[op as usize].get()
    }

    /// The input walks over the packs that did not end within their deadline
    /// ([`PackSettings::input_walk`](crate::PackSettings::input_walk)).
    #[must_use]
    pub fn walks_cut(&self) -> u64 {
        self.walks_cut.get()
    }

    /// One input walk did not end within its deadline.
    pub(crate) fn walk_cut(&self) {
        self.walks_cut.add(1);
    }

    /// Whether the store is taken as down: a call spent its budget, and no call has been
    /// answered by it with anything but a retriable failure since.
    #[must_use]
    pub fn down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
    }

    /// Appends `kbf_pack_store_retries_total` by `op` and `cause`,
    /// `kbf_pack_store_wait_seconds` and `kbf_pack_store_waits_exhausted_total` by `op`, and
    /// `kbf_pack_input_walks_cut_total`, to `out`. A family with no retries still renders, with
    /// zeros.
    pub fn render(&self, out: &mut String) {
        let name = "kbf_pack_store_retries_total";
        let _ = writeln!(
            out,
            "# HELP {name} Calls to the packs' object store made again, by op and by the cause \
             of the failure retried"
        );
        let _ = writeln!(out, "# TYPE {name} counter");
        for op in Op::ALL {
            for cause in Cause::ALL {
                let _ = writeln!(
                    out,
                    "{name}{{op=\"{}\",cause=\"{}\"}} {}",
                    op.label(),
                    cause.label(),
                    self.retries(op, cause)
                );
            }
        }
        let name = "kbf_pack_store_wait_seconds";
        let _ = writeln!(
            out,
            "# HELP {name} Waits before a call to the packs' object store was made again, by op"
        );
        let _ = writeln!(out, "# TYPE {name} histogram");
        for op in Op::ALL {
            let labels = format!("op=\"{}\"", op.label());
            self.waits(op).render(out, name, &labels);
        }
        let name = "kbf_pack_store_waits_exhausted_total";
        let _ = writeln!(
            out,
            "# HELP {name} Calls to the packs' object store that were still failing when no \
             retry was left in their budget, or while it was taken as down, by op"
        );
        let _ = writeln!(out, "# TYPE {name} counter");
        for op in Op::ALL {
            let _ = writeln!(
                out,
                "{name}{{op=\"{}\"}} {}",
                op.label(),
                self.exhausted(op)
            );
        }
        let name = "kbf_pack_input_walks_cut_total";
        let _ = writeln!(
            out,
            "# HELP {name} Executes whose walk of their input tree over the packs did not end \
             within its deadline, answered UNAVAILABLE"
        );
        let _ = writeln!(out, "# TYPE {name} counter");
        let _ = writeln!(out, "{name} {}", self.walks_cut());
    }
}

/// How a wait of a nominal length is jittered: given the nominal wait, the wait taken. The
/// start's retry ([`crate::start_retry`]) takes it.
pub(crate) type Jitter = Arc<dyn Fn(Duration) -> Duration + Send + Sync>;

/// A source of random draws for the jitter.
pub(crate) type Draws = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The draws of a running frontend: a generator seeded by the time it was made.
#[must_use]
pub(crate) fn draws() -> Draws {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let seed = since.map_or(0, |since| since.as_nanos() as u64) ^ u64::from(std::process::id());
    let state = AtomicU64::new(seed);
    Arc::new(move || {
        // splitmix64: one step of the state, mixed. The draw needs to spread waits apart, not
        // to be unpredictable.
        let mut z = state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// The jitter of the start's retry: a nominal `w` becomes a wait between `w / 2` and `w`.
#[must_use]
pub(crate) fn spread() -> Jitter {
    let draw = draws();
    Arc::new(move |nominal| half_to_whole(nominal, draw()))
}

/// The wait between `nominal / 2` and `nominal` that the draw `z` picks.
pub(crate) fn half_to_whole(nominal: Duration, z: u64) -> Duration {
    let half = RetryPolicy {
        jitter_percent: 50,
        ..RetryPolicy::DEFAULT
    };
    half.jittered(nominal, z)
}

/// The jitter of a test that wants the nominal waits.
#[must_use]
pub(crate) fn no_jitter() -> Jitter {
    Arc::new(|nominal| nominal)
}

/// The engine of a [`RetryPolicy`]: its numbers, the clock it reads and waits on, its draws,
/// and where its retries are counted. Cheap to clone.
#[derive(Clone)]
pub struct Backoff {
    policy: RetryPolicy,
    clock: Arc<dyn RetryClock>,
    draws: Draws,
    counted: Arc<StoreWaits>,
}

impl fmt::Debug for Backoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backoff")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl Backoff {
    /// The retries `policy` says, on the tokio timer, counted in `counted`.
    #[must_use]
    pub fn new(policy: RetryPolicy, counted: Arc<StoreWaits>) -> Self {
        Self::with(policy, tokio_clock(), counted)
    }

    /// [`Backoff::new`] reading and waiting on `clock` (a test's virtual clock; it takes a
    /// policy with no jitter to see the nominal waits).
    #[must_use]
    pub fn with(policy: RetryPolicy, clock: Arc<dyn RetryClock>, counted: Arc<StoreWaits>) -> Self {
        Self {
            policy,
            clock,
            draws: draws(),
            counted,
        }
    }

    /// This engine, its retries counted in `counted`.
    #[must_use]
    pub fn counted_in(self, counted: Arc<StoreWaits>) -> Self {
        Self { counted, ..self }
    }

    /// The policy this engine runs.
    #[must_use]
    pub const fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// The retries of one call of `op`, its first attempt about to be made.
    pub(crate) fn call(&self, op: Op) -> Waits<'_> {
        Waits {
            backoff: self,
            op,
            began: self.clock.now(),
            nominal: self.policy.first.min(self.policy.longest),
            attempts: 1,
            down: false,
        }
    }
}

/// What the store's answer to one attempt of a call means for the call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The call was waited for: make it again.
    Again,
    /// The answer is the call's answer: it is not retried, or no retry fits in the budget.
    Answer,
}

/// The retries of one call so far.
pub(crate) struct Waits<'a> {
    backoff: &'a Backoff,
    op: Op,
    /// When the first attempt began, on the engine's clock.
    began: Duration,
    nominal: Duration,
    attempts: u32,
    /// Whether the call's last failure was answered at once, the store being taken as down.
    down: bool,
}

impl Waits<'_> {
    /// After the store answered an attempt with `error`: waits before the next attempt and says
    /// [`Next::Again`], or says [`Next::Answer`].
    pub(crate) async fn after(&mut self, error: &ObjectStoreError) -> Next {
        let Some(cause) = Cause::of(error) else {
            // An answer of the store says it is up; a request that got none says nothing, and
            // a request that got none is retried, so it does not reach here.
            self.answered();
            return Next::Answer;
        };
        if unretried_here() {
            // A caller with a retry of its own around this call (the start's) makes it again.
            return Next::Answer;
        }
        let counted = &self.backoff.counted;
        let op = self.op as usize;
        if counted.down() && self.attempts == 1 {
            counted.exhausted[op].add(1);
            self.down = true;
            return Next::Answer;
        }
        let policy = &self.backoff.policy;
        let left = policy.budget.saturating_sub(self.tried());
        let wait = policy.jittered(self.nominal, (self.backoff.draws)());
        if left.is_zero() || wait > left {
            counted.exhausted[op].add(1);
            counted.down.store(true, Ordering::Relaxed);
            return Next::Answer;
        }
        tracing::debug!(
            op = self.op.label(),
            cause = cause.label(),
            attempt = self.attempts,
            ?wait,
            %error,
            "a call to the pack store failed; trying it again"
        );
        counted.retries[op][cause as usize].add(1);
        counted.seconds[op].observe_duration(wait);
        self.backoff.clock.sleep(wait).await;
        self.nominal = policy.grown(self.nominal);
        self.attempts += 1;
        Next::Again
    }

    /// How long the call has been tried, from its first attempt.
    fn tried(&self) -> Duration {
        self.backoff.clock.now().saturating_sub(self.began)
    }

    /// After the store answered the call with anything but a retriable failure: the store is
    /// up.
    pub(crate) fn answered(&self) {
        self.backoff.counted.down.store(false, Ordering::Relaxed);
    }

    /// `error`, the last answer of a call, saying why it is the answer: its kind is kept, so its
    /// callers still take it for what it is (a 503 keeps its status and code, a request that got
    /// no answer stays one), and its message says how many attempts were made and over how
    /// long, or that it was not retried, the store being taken as down. An answer that is not
    /// retried is returned as it is.
    pub(crate) fn exhausted(&self, error: ObjectStoreError) -> ObjectStoreError {
        let retried = Cause::of(&error).is_some();
        let why = match (self.attempts, self.down) {
            (_, true) => format!(
                " (not retried: a call to the store spent its budget of {} s still failing, and \
                 no call since was answered)",
                self.backoff.policy.budget.as_secs()
            ),
            (1, false) => return error,
            (attempts, false) => format!(
                " (still failing after {attempts} attempts over {:.1} s, the budget being {} s)",
                self.tried().as_secs_f64(),
                self.backoff.policy.budget.as_secs()
            ),
        };
        match error {
            ObjectStoreError::Service {
                status,
                code,
                message,
            } if retried => ObjectStoreError::Service {
                status,
                code,
                message: format!("{message}{why}"),
            },
            ObjectStoreError::Transport(source) => {
                ObjectStoreError::Transport(Arc::new(Retried { source, why }))
            }
            // An answer that is not retried, after retries of others (a 503, then a 404): the
            // store's answer, as it gave it.
            other => other,
        }
    }
}

tokio::task_local! {
    /// Set while a caller that retries its calls itself makes one attempt ([`unretried`]).
    static UNRETRIED: ();
}

/// Runs `attempt` with the retry of the store's calls off: every call to the store under it is
/// made once, and its first failure is its answer, neither counted as exhausted nor taking the
/// store as down. For a caller that has a retry of its own around the attempt, with its own
/// bound (the start's, [`crate::start_retry`]), so that one call is retried by one policy, not
/// by one inside the other.
pub(crate) async fn unretried<T>(attempt: impl std::future::Future<Output = T>) -> T {
    UNRETRIED.scope((), attempt).await
}

/// Whether the call being made is under [`unretried`].
fn unretried_here() -> bool {
    UNRETRIED.try_with(|()| ()).is_ok()
}

/// A request that got no answer on its last attempt, after the call was retried.
#[derive(Debug)]
struct Retried {
    source: Arc<dyn Error + Send + Sync>,
    why: String,
}

impl fmt::Display for Retried {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The text of a transport failure carries each of its causes and no `source`
        // (`ObjectStoreError::Transport` prints the chain of what it holds), so this does too:
        // the source's chain, then why.
        write!(f, "{}", self.source)?;
        let mut next = self.source.source();
        while let Some(cause) = next {
            write!(f, ": {cause}")?;
            next = cause.source();
        }
        write!(f, "{}", self.why)
    }
}

impl Error for Retried {}

/// An object store whose calls are retried, as the module says.
pub struct Waiting<S> {
    inner: Arc<S>,
    backoff: Backoff,
}

impl<S> Waiting<S> {
    /// `inner`, its calls retried as `backoff` says.
    #[must_use]
    pub const fn new(inner: Arc<S>, backoff: Backoff) -> Self {
        Self { inner, backoff }
    }
}

impl<S> std::fmt::Debug for Waiting<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waiting")
            .field("policy", &self.backoff.policy)
            .finish_non_exhaustive()
    }
}

/// Makes the call `$call` of `$self`'s store, as `$op`, until it is answered as [`Waits::after`]
/// says.
macro_rules! waiting {
    ($self:ident, $op:expr, $call:expr) => {{
        let mut waits = $self.backoff.call($op);
        loop {
            match $call.await {
                Err(error) => match waits.after(&error).await {
                    Next::Again => continue,
                    Next::Answer => break Err(waits.exhausted(error)),
                },
                answer => {
                    waits.answered();
                    break answer;
                }
            }
        }
    }};
}

impl<S: ObjectStore> ObjectStore for Waiting<S> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        waiting!(
            self,
            Op::Put,
            self.inner.put_new(key, body.clone(), retain_until)
        )
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_stream(key, len, body).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        waiting!(self, Op::Get, self.inner.get_range(key, range))
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        waiting!(self, Op::Head, self.inner.may_hold(key))
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        waiting!(self, Op::Head, self.inner.len(key))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        waiting!(self, Op::Delete, self.inner.delete(key))
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        waiting!(self, Op::List, self.inner.list(prefix, after, max_keys))
    }
}

#[cfg(test)]
#[path = "store_wait_tests.rs"]
mod tests;
