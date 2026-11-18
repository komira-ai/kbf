//! The frontend's wait for a store that answers 503: [`Waiting`], which
//! [`Packs`](crate::Packs) puts in front of its store, so that every call the frontend makes of
//! the packs' store (client uploads and reads, the executor's preparation and ingest, base
//! publication, the janitor's listings and deletes) rides out a store that is briefly
//! unavailable, and [`Backoff`], the schedule of those waits, which the action trees' own retry
//! ([`PutRetry`](crate::action_tree::PutRetry)) shares.
//!
//! - **What is waited out.** A 503 of the store ([`ObjectStoreError::Service`] with status 503):
//!   a store answers one while it fails over its metadata (up to about 30 s, for writes,
//!   deletes, listings and lookups of keys it has to find), and a `SlowDown` while it is taking
//!   as many uploads as it can. Every other answer is the call's answer at once.
//! - **The waits.** The first wait is about [`FIRST_WAIT`] and each one after is about twice the
//!   one before, up to [`LONGEST_WAIT`]. Each is jittered: a wait of a nominal `w` lasts
//!   between `w / 2` and `w`, so the callers a failover stopped together do not come back
//!   together. The store's `Retry-After` is not read: the store client gives kbf no header of
//!   an error answer. The waits of one call add up to at most [`STORE_WAIT`] (60 s); the wait
//!   that would pass it is cut to end there, and a 503 when the waits have added up to it is
//!   the call's answer, with the time waited in its message. A failover of 30 s is ridden out
//!   with half of the bound to spare.
//! - **A store that is down.** Once a call has waited out the bound with the store still
//!   answering 503, the store is taken as down: every 503 after that is its call's answer at
//!   once, with no wait, until the store answers a call with anything but a 503, which takes it
//!   as up again. So a store down for good costs the bound once, not once per call: a pack
//!   written again under another key after a failed write, the delete of the failed key, the
//!   next of a preparation's reads, are each answered at once. The mark is shared by every
//!   caller that counts its waits in the same [`StoreWaits`] (the packs, the action trees'
//!   retry, the janitor).
//! - **`SlowDown` of a write.** A write whose body is in hand ([`ObjectStore::put_new`]) waits out
//!   `SlowDown` for as long as the store answers it, with the same waits, which do not count
//!   toward the bound: it is back-pressure, not a fault, and what ends it is the caller (a
//!   client's `grpc-timeout`, a preparation's limit). A `SlowDown` of a read, a lookup, a
//!   listing or a delete is a 503 like any other.
//! - **A streamed write** ([`ObjectStore::put_stream`]) is made once: its body cannot be read
//!   twice. Its caller repeats it if it can make the body again (an action tree's write reads
//!   its source again, with [`Backoff`]); a client's upload over the packs' bound cannot, and
//!   the store's answer ends it UNAVAILABLE, which the client repeats. Such an upload asks the
//!   store whether it holds the object first ([`ObjectStore::len`]), and that lookup waits out
//!   the 503 of a store that is failing over, so a repeated upload waits before it is sent.
//! - **Metrics.** Every wait is recorded in [`StoreWaits`] by the [`Op`] of the call: how many
//!   and how long (`kbf_pack_store_wait_seconds`), and the calls that still answered 503 when
//!   their waits reached the bound (`kbf_pack_store_waits_exhausted_total`).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use crate::action_tree::{FIRST_WAIT, LONGEST_WAIT, Sleeper, is_slow_down, tokio_sleeper};
use crate::metrics::{Counter, Histogram, Unit};
use crate::store_metrics::Op;

/// The most the waits of one call for a store that answers 503 add up to.
pub const STORE_WAIT: Duration = Duration::from_secs(60);

/// What the waits for the store have been: by [`Op`], each wait and its length, and the calls
/// whose waits reached the bound with the store still answering 503; and whether the store is
/// taken as down (as the module says).
#[derive(Debug)]
pub struct StoreWaits {
    seconds: [Histogram; Op::ALL.len()],
    exhausted: [Counter; Op::ALL.len()],
    down: AtomicBool,
}

impl Default for StoreWaits {
    fn default() -> Self {
        Self {
            seconds: std::array::from_fn(|_| Histogram::new(Unit::Seconds)),
            exhausted: Default::default(),
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

    /// The calls of `op` whose waits reached the bound with the store still answering 503, and
    /// those answered 503 at once while the store was taken as down.
    #[must_use]
    pub fn exhausted(&self, op: Op) -> u64 {
        self.exhausted[op as usize].get()
    }

    /// Whether the store is taken as down: a call waited out the bound, and no call has been
    /// answered by it with anything but a 503 since.
    #[must_use]
    pub fn down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
    }

    /// Appends `kbf_pack_store_wait_seconds` and `kbf_pack_store_waits_exhausted_total`, by
    /// `op`, to `out`. A family with no waits still renders, with zeros.
    pub fn render(&self, out: &mut String) {
        let name = "kbf_pack_store_wait_seconds";
        let _ = writeln!(
            out,
            "# HELP {name} Waits for the packs' object store after it answered 503, by op"
        );
        let _ = writeln!(out, "# TYPE {name} histogram");
        for op in Op::ALL {
            let labels = format!("op=\"{}\"", op.label());
            self.waits(op).render(out, name, &labels);
        }
        let name = "kbf_pack_store_waits_exhausted_total";
        let _ = writeln!(
            out,
            "# HELP {name} Calls to the packs' object store that still answered 503 when their \
             waits reached the bound, or while it was taken as down, by op"
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
    }
}

/// How a wait of a nominal length is jittered: given the nominal wait, the wait taken.
pub(crate) type Jitter = Arc<dyn Fn(Duration) -> Duration + Send + Sync>;

/// The jitter of a running frontend: a nominal `w` becomes a wait between `w / 2` and `w`,
/// drawn from a generator seeded by the time the jitter was made.
#[must_use]
pub(crate) fn spread() -> Jitter {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let seed = since.map_or(0, |since| since.as_nanos() as u64) ^ u64::from(std::process::id());
    let state = AtomicU64::new(seed);
    Arc::new(move |nominal| {
        // splitmix64: one step of the state, mixed. The draw needs to spread waits apart, not
        // to be unpredictable.
        let mut z = state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        half_to_whole(nominal, z)
    })
}

/// The wait between `nominal / 2` and `nominal` that the draw `z` picks.
pub(crate) fn half_to_whole(nominal: Duration, z: u64) -> Duration {
    let half = nominal / 2;
    let span = u64::try_from((nominal - half).as_nanos()).unwrap_or(u64::MAX);
    half + Duration::from_nanos(z.checked_rem(span.saturating_add(1)).unwrap_or(0))
}

/// The jitter of a test that wants the nominal waits.
#[must_use]
pub(crate) fn no_jitter() -> Jitter {
    Arc::new(|nominal| nominal)
}

/// The schedule of the waits for a store that answers 503: their bound, how a wait is taken and
/// jittered, and where it is counted. Cheap to clone.
#[derive(Clone)]
pub struct Backoff {
    total: Duration,
    sleeper: Arc<dyn Sleeper>,
    jitter: Jitter,
    counted: Arc<StoreWaits>,
}

impl Backoff {
    /// Waits that add up to at most `total` for one call, taken on the tokio timer, jittered and
    /// counted in `counted`.
    #[must_use]
    pub fn new(total: Duration, counted: Arc<StoreWaits>) -> Self {
        Self::with(total, tokio_sleeper(), spread(), counted)
    }

    /// [`Backoff::new`] waiting with `sleeper` and jittered by `jitter` (a test records the waits
    /// and takes them as they are).
    #[must_use]
    pub(crate) fn with(
        total: Duration,
        sleeper: Arc<dyn Sleeper>,
        jitter: Jitter,
        counted: Arc<StoreWaits>,
    ) -> Self {
        Self {
            total,
            sleeper,
            jitter,
            counted,
        }
    }

    /// This schedule, its waits counted in `counted`.
    #[must_use]
    pub fn counted_in(self, counted: Arc<StoreWaits>) -> Self {
        Self { counted, ..self }
    }

    /// The most the waits of one call add up to.
    #[must_use]
    pub(crate) const fn total(&self) -> Duration {
        self.total
    }

    /// The waits of one call of `op`, none taken yet.
    pub(crate) fn call(&self, op: Op) -> Waits<'_> {
        Waits {
            backoff: self,
            op,
            nominal: FIRST_WAIT,
            waited: Duration::ZERO,
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
    /// The answer is the call's answer: it is not a 503, or the waits have reached the bound.
    Answer,
}

/// The waits of one call so far.
pub(crate) struct Waits<'a> {
    backoff: &'a Backoff,
    op: Op,
    nominal: Duration,
    waited: Duration,
    attempts: u32,
    /// Whether the call's last 503 was answered at once, the store being taken as down.
    down: bool,
}

impl Waits<'_> {
    /// After the store answered an attempt with `error`: waits before the next attempt and says
    /// [`Next::Again`], or says [`Next::Answer`]. `endless_slow_down` makes a `SlowDown` waited
    /// out without counting toward the bound (a write whose body is in hand).
    pub(crate) async fn after(
        &mut self,
        error: &ObjectStoreError,
        endless_slow_down: bool,
    ) -> Next {
        if !matches!(error, ObjectStoreError::Service { status: 503, .. }) {
            // An answer of the store says it is up; a request that got none says nothing.
            if !matches!(error, ObjectStoreError::Transport(_)) {
                self.answered();
            }
            return Next::Answer;
        }
        let endless = endless_slow_down && is_slow_down(error);
        let counted = &self.backoff.counted;
        let wait = match endless {
            true => (self.backoff.jitter)(self.nominal),
            false if counted.down() && self.waited.is_zero() => {
                counted.exhausted[self.op as usize].add(1);
                self.down = true;
                return Next::Answer;
            }
            false if self.waited >= self.backoff.total => {
                counted.exhausted[self.op as usize].add(1);
                counted.down.store(true, Ordering::Relaxed);
                return Next::Answer;
            }
            false => (self.backoff.jitter)(self.nominal).min(self.backoff.total - self.waited),
        };
        tracing::debug!(op = self.op.label(), ?wait, %error, "the store answered 503; waiting");
        self.backoff.counted.seconds[self.op as usize].observe_duration(wait);
        self.backoff.sleeper.sleep(wait).await;
        if !endless {
            self.waited += wait;
        }
        self.nominal = (self.nominal * 2).min(LONGEST_WAIT);
        self.attempts += 1;
        Next::Again
    }

    /// After the store answered the call with anything but a 503: the store is up.
    pub(crate) fn answered(&self) {
        self.backoff.counted.down.store(false, Ordering::Relaxed);
    }

    /// `error`, the last answer of a call, saying why it is the answer: a 503 keeps its status
    /// and code, so its callers still take it for one, and its message says how long the call
    /// waited and how many times it was made, or that it was not waited for, the store being
    /// taken as down.
    pub(crate) fn exhausted(&self, error: ObjectStoreError) -> ObjectStoreError {
        let ObjectStoreError::Service {
            status: 503,
            code,
            message,
        } = error
        else {
            return error;
        };
        let why = match (self.waited.is_zero(), self.down) {
            (false, _) => format!(
                " (still so after {} attempts and {:.1} s of waits)",
                self.attempts,
                self.waited.as_secs_f64()
            ),
            (true, true) => format!(
                " (not waited for: the store answered 503 past the bound of {} s of waits, and \
                 no call since)",
                self.backoff.total.as_secs()
            ),
            (true, false) => String::new(),
        };
        ObjectStoreError::Service {
            status: 503,
            code,
            message: format!("{message}{why}"),
        }
    }
}

/// An object store whose calls wait out its 503s, as the module says.
pub struct Waiting<S> {
    inner: Arc<S>,
    backoff: Backoff,
}

impl<S> Waiting<S> {
    /// `inner`, its calls waiting as `backoff` says.
    #[must_use]
    pub const fn new(inner: Arc<S>, backoff: Backoff) -> Self {
        Self { inner, backoff }
    }
}

impl<S> std::fmt::Debug for Waiting<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waiting")
            .field("total", &self.backoff.total)
            .finish_non_exhaustive()
    }
}

/// Makes the call `$call` of `$self`'s store, as `$op`, until it is answered as [`Waits::after`]
/// says.
macro_rules! waiting {
    ($self:ident, $op:expr, $endless:expr, $call:expr) => {{
        let mut waits = $self.backoff.call($op);
        loop {
            match $call.await {
                Err(error) => match waits.after(&error, $endless).await {
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
            true,
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
        waiting!(self, Op::Get, false, self.inner.get_range(key, range))
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        waiting!(self, Op::Head, false, self.inner.may_hold(key))
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        waiting!(self, Op::Head, false, self.inner.len(key))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        waiting!(self, Op::Delete, false, self.inner.delete(key))
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        waiting!(
            self,
            Op::List,
            false,
            self.inner.list(prefix, after, max_keys)
        )
    }
}

#[cfg(test)]
#[path = "store_wait_tests.rs"]
mod tests;
