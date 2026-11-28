//! [`Waiting`] and [`Backoff`]: every call to a store that fails for a while (no answer, a 503
//! `SlowDown` or not, a 500, 502 or 504, a 400 `RequestTimeout`) is made again after jittered
//! waits until the store answers, within the policy's budget, which counts the time inside the
//! attempts too; a store still failing when no retry fits is the call's answer, saying so; every
//! other answer is the call's at once; every retry is counted by its op and cause. Time is a
//! virtual clock that the fake moves by each wait it is asked for (and by the time an attempt is
//! scripted to take), so a window of 30 s is 30 s of waits, counted, never taken.

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use futures::future::BoxFuture;
use futures::stream;
use kbf_objstore::MemoryStore;

use super::*;

/// The virtual clock: the waits taken so far and the time attempts took, added up.
#[derive(Default)]
struct Clock {
    now: Mutex<Duration>,
    waits: Mutex<Vec<Duration>>,
}

impl Clock {
    fn waits(&self) -> Vec<Duration> {
        self.waits.lock().expect("not poisoned").clone()
    }

    /// Time passes with no wait taken: an attempt that took `by`.
    fn pass(&self, by: Duration) {
        *self.now.lock().expect("not poisoned") += by;
    }
}

impl RetryClock for Clock {
    fn now(&self) -> Duration {
        *self.now.lock().expect("not poisoned")
    }

    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        *self.now.lock().expect("not poisoned") += wait;
        self.waits.lock().expect("not poisoned").push(wait);
        Box::pin(std::future::ready(()))
    }
}

/// A memory store that answers every call `answer` until the clock reaches `until`, counts the
/// calls it was asked, and lets each take `takes` of the clock's time before it answers.
struct Failing {
    inner: MemoryStore,
    clock: Arc<Clock>,
    until: Duration,
    answer: fn() -> ObjectStoreError,
    takes: Duration,
    calls: AtomicUsize,
}

impl Failing {
    fn new(clock: &Arc<Clock>, until: Duration, answer: fn() -> ObjectStoreError) -> Arc<Self> {
        Self::taking(clock, until, answer, Duration::ZERO)
    }

    fn taking(
        clock: &Arc<Clock>,
        until: Duration,
        answer: fn() -> ObjectStoreError,
        takes: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            clock: Arc::clone(clock),
            until,
            answer,
            takes,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The scripted answer while the store is failing, else `None`.
    fn refused(&self) -> Option<ObjectStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let failing = self.clock.now() < self.until;
        self.clock.pass(self.takes);
        failing.then(self.answer)
    }
}

impl ObjectStore for Failing {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.put_new(key, body, retain_until).await,
        }
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.put_stream(key, len, body).await,
        }
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.get_range(key, range).await,
        }
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.may_hold(key).await,
        }
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.len(key).await,
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.delete(key).await,
        }
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        match self.refused() {
            Some(error) => Err(error),
            None => self.inner.list(prefix, after, max_keys).await,
        }
    }
}

/// A metadata failover's answer.
fn unavailable() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 503,
        code: "ServiceUnavailable".to_owned(),
        message: "injected".to_owned(),
    }
}

/// The store's back-pressure.
fn slow_down() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 503,
        code: "SlowDown".to_owned(),
        message: "injected".to_owned(),
    }
}

fn internal() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 500,
        code: "InternalError".to_owned(),
        message: "injected".to_owned(),
    }
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/test/{name}")).expect("a key")
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// A request that got no answer.
fn reset() -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(std::io::Error::other("reset")))
}

/// The default policy with no jitter, so that the waits are the nominal ones, and `budget`.
fn policy_without_jitter(budget: Duration) -> RetryPolicy {
    RetryPolicy {
        jitter_percent: 0,
        ..RetryPolicy::DEFAULT.with_budget(budget)
    }
}

/// `store`, waiting on `clock` with no jitter for at most `budget`, counted in `waits`.
fn waiting(
    store: &Arc<Failing>,
    clock: &Arc<Clock>,
    budget: Duration,
    waits: &Arc<StoreWaits>,
) -> Waiting<Failing> {
    let clock = Arc::clone(clock) as Arc<dyn RetryClock>;
    let backoff = Backoff::with(policy_without_jitter(budget), clock, Arc::default());
    Waiting::new(Arc::clone(store), backoff.counted_in(Arc::clone(waits)))
}

/// The nominal waits of a call to a store that fails over for 30 s: 0.5, 1, 2, 4, 8, 8 and 8 s,
/// which pass 30 s at the seventh (31.5 s).
const RIDDEN: [f64; 7] = [0.5, 1.0, 2.0, 4.0, 8.0, 8.0, 8.0];

/// Catches a call the store answers 503 during a failover of 30 s that fails instead of waiting:
/// each of a write, a ranged read, a length, a presence check, a delete and a listing, made when
/// the failover begins, is made again after the doubling waits until the store answers, and is
/// answered as the store answers once it is back; every wait is counted by its op, and every
/// retry under the cause `unavailable`.
#[tokio::test]
async fn every_call_rides_out_a_failover_of_30_seconds() {
    let window = Duration::from_secs(30);
    let want: Vec<Duration> = RIDDEN.map(secs).to_vec();
    let prefix = KeyPrefix::new("cas/test/").expect("a prefix");
    let range = ByteRange::new(0, 3).expect("a range");

    let ops = [Op::Put, Op::Get, Op::Head, Op::Head, Op::List, Op::Delete];
    for (n, op) in ops.into_iter().enumerate() {
        let clock = Arc::new(Clock::default());
        let failing = Failing::new(&clock, window, unavailable);
        failing
            .inner
            .put_new(&key("held"), Bytes::from_static(b"abc"), None)
            .await
            .expect("seeded");
        let waits = Arc::new(StoreWaits::default());
        let over = waiting(&failing, &clock, STORE_WAIT, &waits);

        let said = match n {
            0 => format!(
                "{:?}",
                over.put_new(&key("new"), Bytes::from_static(b"n"), None)
                    .await
            ),
            1 => format!("{:?}", over.get_range(&key("held"), range).await),
            2 => format!("{:?}", over.len(&key("held")).await),
            3 => format!("{:?}", over.may_hold(&key("held")).await),
            4 => format!(
                "{:?}",
                over.list(&prefix, None, PageSize::MAX)
                    .await
                    .map(|page| page.objects.len())
            ),
            _ => format!("{:?}", over.delete(&key("held")).await),
        };

        let answered = [
            "Ok(())",
            "Ok(b\"abc\")",
            "Ok(Some(3))",
            "Ok(true)",
            "Ok(1)",
            "Ok(())",
        ];
        assert_eq!(said, answered[n], "op {n}");
        assert_eq!(clock.waits(), want, "op {n}");
        assert_eq!(failing.calls(), want.len() + 1, "op {n}");
        assert_eq!(waits.waits(op).count(), want.len() as u64, "op {n}");
        assert_eq!(waits.waits(op).sum(), 31_500_000_000, "op {n}");
        assert_eq!(waits.exhausted(op), 0, "op {n}");
        assert_eq!(
            waits.retries(op, Cause::Unavailable),
            want.len() as u64,
            "op {n}"
        );
        assert!(!waits.down(), "op {n}");
    }
}

/// Catches a call that waits without end for a store that stays down, or ends without saying
/// why: a read of a store that answers 503 past the budget of 120 s is made 19 times, after the
/// nominal waits (0.5, 1, 2, 4, then 8 s fourteen times: 119.5 s; the next 8 s would pass 120 s,
/// so it is not taken), and its answer is the store's 503, its code kept, saying how many attempts
/// it made over how long; the call is counted as exhausted and the store is taken as down.
#[tokio::test]
async fn a_store_down_past_the_budget_is_its_503_saying_so() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, STORE_WAIT, &waits);
    let range = ByteRange::new(0, 1).expect("a range");

    let answer = over.get_range(&key("gone"), range).await;

    let error = answer.expect_err("refused");
    assert_eq!(
        error.to_string(),
        "store answered 503 ServiceUnavailable: injected (still failing after 19 attempts over \
         119.5 s, the budget being 120 s)"
    );
    assert!(
        matches!(&error, ObjectStoreError::Service { status: 503, code, .. } if code == "ServiceUnavailable"),
        "{error:?}"
    );
    let mut nominal = vec![0.5, 1.0, 2.0, 4.0];
    nominal.extend([8.0; 14]);
    let nominal: Vec<Duration> = nominal.into_iter().map(secs).collect();
    assert_eq!(clock.waits(), nominal);
    assert_eq!(clock.now(), secs(119.5));
    assert_eq!(store.calls(), 19);
    assert_eq!(waits.exhausted(Op::Get), 1);
    assert_eq!(waits.waits(Op::Get).count(), 18);
    assert!(waits.down());
}

/// Catches a budget of zero that still waits, or a 503 rewritten when nothing was waited for:
/// with no budget, a 503 is the call's answer at once, as the store gave it, counted as
/// exhausted.
#[tokio::test]
async fn a_budget_of_zero_answers_the_first_failure() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::ZERO, &waits);

    let answer = over.len(&key("gone")).await;

    assert_eq!(
        answer.expect_err("refused").to_string(),
        unavailable().to_string()
    );
    assert_eq!(store.calls(), 1);
    assert!(clock.waits().is_empty());
    assert_eq!(waits.exhausted(Op::Head), 1);
    assert_eq!(waits.retries(Op::Head, Cause::Unavailable), 0);
}

/// Catches a `SlowDown` that is waited out without end (as it was, for a whole write), or one
/// that is not waited at all: it counts toward the budget like any other failure. With a budget
/// of 1 s, a write answered `SlowDown` is retried once (0.5 s; the next wait, 1 s, would pass the
/// budget) and answered `SlowDown` saying so; with the budget of 120 s, a write answered it for
/// 20 s is made after waits of 0.5, 1, 2, 4, 8 and 8 s and stored. Both are counted under
/// `slow_down`.
#[tokio::test]
async fn a_slow_down_counts_toward_the_budget() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, slow_down);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::from_secs(1), &waits);

    let error = over
        .put_new(&key("w"), Bytes::from_static(b"w"), None)
        .await
        .expect_err("refused");

    assert_eq!(
        error.to_string(),
        "store answered 503 SlowDown: injected (still failing after 2 attempts over 0.5 s, the \
         budget being 1 s)"
    );
    assert_eq!(clock.waits(), [0.5].map(secs));
    assert_eq!(store.calls(), 2);
    assert_eq!(waits.exhausted(Op::Put), 1);
    assert_eq!(waits.retries(Op::Put, Cause::SlowDown), 1);

    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::from_secs(20), slow_down);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, STORE_WAIT, &waits);
    over.put_new(&key("w"), Bytes::from_static(b"w"), None)
        .await
        .expect("stored");
    assert_eq!(clock.waits(), [0.5, 1.0, 2.0, 4.0, 8.0, 8.0].map(secs));
    assert_eq!(waits.exhausted(Op::Put), 0);
    assert_eq!(waits.retries(Op::Put, Cause::SlowDown), 6);
}

/// Catches a failure the store may recover from taken as the call's answer: a request that got
/// no answer, a 500, a 502, a 504 and a 400 `RequestTimeout` are each made again after waits of
/// 0.5 and 1 s until the store answers (at 1 s), and each retry is counted under its own cause and
/// no other.
#[tokio::test]
async fn every_retried_failure_is_waited_out_and_counted_under_its_cause() {
    fn bad_gateway() -> ObjectStoreError {
        ObjectStoreError::Service {
            status: 502,
            code: "BadGateway".to_owned(),
            message: "injected".to_owned(),
        }
    }
    fn gateway_timeout() -> ObjectStoreError {
        ObjectStoreError::Service {
            status: 504,
            code: "GatewayTimeout".to_owned(),
            message: "injected".to_owned(),
        }
    }
    fn request_timeout() -> ObjectStoreError {
        ObjectStoreError::Service {
            status: 400,
            code: "RequestTimeout".to_owned(),
            message: "injected".to_owned(),
        }
    }
    let cases: [(fn() -> ObjectStoreError, Cause); 6] = [
        (reset, Cause::Connection),
        (internal, Cause::ServerError),
        (bad_gateway, Cause::ServerError),
        (gateway_timeout, Cause::ServerError),
        (request_timeout, Cause::RequestTimeout),
        (slow_down, Cause::SlowDown),
    ];
    for (answer, cause) in cases {
        let clock = Arc::new(Clock::default());
        let store = Failing::new(&clock, Duration::from_secs(1), answer);
        let waits = Arc::new(StoreWaits::default());
        let over = waiting(&store, &clock, STORE_WAIT, &waits);
        store
            .inner
            .put_new(&key("x"), Bytes::from_static(b"x"), None)
            .await
            .expect("seeded");

        over.delete(&key("x")).await.expect("deleted");

        let label = cause.label();
        assert_eq!(store.calls(), 3, "{label}");
        assert_eq!(clock.waits(), [0.5, 1.0].map(secs), "{label}");
        for other in Cause::ALL {
            let want = if other == cause { 2 } else { 0 };
            assert_eq!(waits.retries(Op::Delete, other), want, "{label}: {other:?}");
        }
        assert_eq!(waits.exhausted(Op::Delete), 0, "{label}");
    }
}

/// Catches an answer the store gives for good that is waited for: a 403, a 404 status, a 400 of
/// another code, a missing object and a refused replacement are each the call's answer on its
/// first attempt, with no wait, no retry counted and the store taken as up.
#[tokio::test]
async fn answers_given_for_good_are_the_call_s_at_once() {
    fn denied() -> ObjectStoreError {
        ObjectStoreError::Service {
            status: 403,
            code: "AccessDenied".to_owned(),
            message: "injected".to_owned(),
        }
    }
    fn no_bucket() -> ObjectStoreError {
        ObjectStoreError::Service {
            status: 404,
            code: "NoSuchBucket".to_owned(),
            message: "injected".to_owned(),
        }
    }
    fn invalid() -> ObjectStoreError {
        ObjectStoreError::Service {
            status: 400,
            code: "InvalidArgument".to_owned(),
            message: "injected".to_owned(),
        }
    }
    for answer in [denied as fn() -> ObjectStoreError, no_bucket, invalid] {
        let clock = Arc::new(Clock::default());
        let store = Failing::new(&clock, Duration::MAX, answer);
        let waits = Arc::new(StoreWaits::default());
        let over = waiting(&store, &clock, STORE_WAIT, &waits);

        let said = over.delete(&key("x")).await.expect_err("refused");

        assert_eq!(said.to_string(), answer().to_string());
        assert_eq!(store.calls(), 1);
        assert!(clock.waits().is_empty());
        assert_eq!(waits.exhausted(Op::Delete), 0);
        for cause in Cause::ALL {
            assert_eq!(waits.retries(Op::Delete, cause), 0, "{cause:?}");
        }
        assert!(!waits.down());
    }
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::ZERO, unavailable);
    let over = waiting(&store, &clock, STORE_WAIT, &Arc::default());
    let range = ByteRange::new(0, 1).expect("a range");
    let missing = over.get_range(&key("none"), range).await;
    assert!(
        matches!(missing, Err(ObjectStoreError::NotFound(_))),
        "{missing:?}"
    );
    over.put_new(&key("y"), Bytes::from_static(b"y"), None)
        .await
        .expect("stored");
    let again = over
        .put_new(&key("y"), Bytes::from_static(b"y"), None)
        .await;
    assert!(
        matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
        "{again:?}"
    );
    assert_eq!(store.calls(), 3);
    assert!(clock.waits().is_empty());
}

/// Catches an attempt's own time left out of the budget (which would let a call of stalled
/// attempts last the budget plus every attempt): against a store whose every call stalls for 60 s
/// (as a stall cut by the client does) and then fails with no answer, a call with a budget of 120 s
/// attempts at 0 s, waits 0.5 s, attempts at 60.5 s (ending at 120.5 s) and is not made again,
/// the answer saying it made 2 attempts over 120.5 s.
#[tokio::test]
async fn an_attempt_s_own_time_counts_toward_the_budget() {
    let clock = Arc::new(Clock::default());
    let store = Failing::taking(&clock, Duration::MAX, reset, Duration::from_secs(60));
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, STORE_WAIT, &waits);

    let error = over.len(&key("slow")).await.expect_err("failed");

    assert_eq!(store.calls(), 2);
    assert_eq!(clock.waits(), [0.5].map(secs));
    assert_eq!(clock.now(), secs(120.5));
    assert_eq!(
        error.to_string(),
        "request to the store failed: reset (still failing after 2 attempts over 120.5 s, the \
         budget being 120 s)"
    );
    assert_eq!(waits.exhausted(Op::Head), 1);
    assert_eq!(waits.retries(Op::Head, Cause::Connection), 1);
    assert!(waits.down());
}

/// Catches a request that got no answer that is rewritten into another kind of error, or whose
/// message loses the cause: after the budget it is still [`ObjectStoreError::Transport`], and its
/// text is the store client's text, the source's text once, then why.
#[tokio::test]
async fn a_transport_failure_keeps_its_kind_and_its_source_text() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, reset);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::from_secs(1), &waits);

    let error = over.len(&key("gone")).await.expect_err("failed");

    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "request to the store failed: reset (still failing after 2 attempts over 0.5 s, the \
         budget being 1 s)"
    );
}

/// Catches a streamed write made again, which would send a body already read: a streamed write
/// the store answers 503 is answered so after one attempt and no wait.
#[tokio::test]
async fn a_streamed_write_is_made_once() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let over = waiting(&store, &clock, STORE_WAIT, &Arc::default());
    let body: BodyStream = Box::pin(stream::iter([Ok(Bytes::from_static(b"s"))]));

    let refused = over.put_stream(&key("s"), 1, body).await;

    assert_eq!(
        refused.expect_err("refused").to_string(),
        unavailable().to_string()
    );
    assert_eq!(store.calls(), 1);
    assert!(clock.waits().is_empty());
    assert_eq!(over.capabilities(), store.capabilities());
    let policy = policy_without_jitter(STORE_WAIT);
    assert_eq!(
        format!("{over:?}"),
        format!("Waiting {{ policy: {policy:?}, .. }}")
    );
}

/// Catches jitter outside half to all of the nominal wait, or none at all: every wait of the
/// running jitter lies between half of the nominal and all of it, and they are not all the same.
#[test]
fn the_jitter_spreads_a_wait_over_its_second_half() {
    let jitter = spread();
    let nominal = Duration::from_secs(8);
    let drawn: Vec<Duration> = (0..256).map(|_| jitter(nominal)).collect();
    assert!(drawn.iter().all(|w| *w >= nominal / 2 && *w <= nominal));
    assert!(drawn.iter().any(|w| *w != drawn[0]));
    assert_eq!(half_to_whole(nominal, 0), nominal);
    assert_eq!(half_to_whole(nominal, 4_000_000_000), nominal / 2);
    assert_eq!(half_to_whole(nominal, 4_000_000_001), nominal);
    assert_eq!(half_to_whole(Duration::ZERO, u64::MAX), Duration::ZERO);
    assert_eq!(no_jitter()(nominal), nominal);
}

/// Catches jitter that lets a call wait past its budget, or a wait outside its nominal wait's
/// second half: with the default 50 % jitter, a call to a store that stays down ends inside its
/// budget of 120 s, every wait lies in [w/2, w] of the nominal wait w of its place (0.5, 1, 2, 4,
/// then 8 s), and the waits are not all the nominal ones.
#[tokio::test]
async fn jittered_waits_stay_in_their_second_half_and_end_inside_the_budget() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let waits = Arc::new(StoreWaits::default());
    let dynamic = Arc::clone(&clock) as Arc<dyn RetryClock>;
    let over = Waiting::new(
        Arc::clone(&store),
        Backoff::with(RetryPolicy::DEFAULT, dynamic, Arc::clone(&waits)),
    );
    let prefix = KeyPrefix::new("cas/").expect("a prefix");

    let answer = over.list(&prefix, None, PageSize::MAX).await;

    assert!(answer.is_err());
    assert!(clock.now() <= STORE_WAIT);
    let taken = clock.waits();
    assert!(taken.len() > 10, "{taken:?}");
    assert_eq!(store.calls(), taken.len() + 1);
    let mut nominal = RetryPolicy::DEFAULT.first;
    for (n, wait) in taken.iter().enumerate() {
        assert!(
            *wait >= nominal / 2 && *wait <= nominal,
            "wait {n}: {wait:?} of {nominal:?}"
        );
        nominal = RetryPolicy::DEFAULT.grown(nominal);
    }
    assert!(taken.iter().skip(5).any(|w| *w != secs(8.0)), "{taken:?}");
}

/// Catches a wait family that is missing from the scrape or counts the wrong op or cause: with no
/// waits every family renders every op (and every op and cause of the retries) with zeros, and a
/// 503 of a read is counted under `op="get",cause="unavailable"` twice, with its two waits (1.5 s
/// in all), and a cut input walk under `kbf_pack_input_walks_cut_total`.
#[tokio::test]
async fn the_waits_render_by_op() {
    let waits = Arc::new(StoreWaits::default());
    let mut zero = String::new();
    waits.render(&mut zero);
    assert!(zero.starts_with(
        "# HELP kbf_pack_store_retries_total Calls to the packs' object store made again, by op \
         and by the cause of the failure retried\n# TYPE kbf_pack_store_retries_total counter\n"
    ));
    for op in Op::ALL {
        for cause in Cause::ALL {
            let line = format!(
                "kbf_pack_store_retries_total{{op=\"{}\",cause=\"{}\"}} 0\n",
                op.label(),
                cause.label()
            );
            assert!(zero.contains(&line), "{line}");
        }
    }
    assert!(zero.contains("# TYPE kbf_pack_store_wait_seconds histogram\n"));
    assert!(zero.contains("kbf_pack_store_wait_seconds_count{op=\"list\"} 0\n"));
    assert!(zero.ends_with(
        "# TYPE kbf_pack_store_waits_exhausted_total counter\n\
         kbf_pack_store_waits_exhausted_total{op=\"get\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"head\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"put\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"delete\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"list\"} 0\n\
         # HELP kbf_pack_input_walks_cut_total Executes whose walk of their input tree over the \
         packs did not end within its deadline, answered UNAVAILABLE\n\
         # TYPE kbf_pack_input_walks_cut_total counter\n\
         kbf_pack_input_walks_cut_total 0\n"
    ));

    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::from_secs(1), unavailable);
    let over = waiting(&store, &clock, STORE_WAIT, &waits);
    store
        .inner
        .put_new(&key("r"), Bytes::from_static(b"r"), None)
        .await
        .expect("seeded");
    let range = ByteRange::new(0, 1).expect("a range");
    over.get_range(&key("r"), range).await.expect("read");
    waits.walk_cut();

    let mut out = String::new();
    waits.render(&mut out);
    assert!(out.contains("kbf_pack_store_wait_seconds_count{op=\"get\"} 2\n"));
    assert!(out.contains("kbf_pack_store_wait_seconds_sum{op=\"get\"} 1.5\n"));
    assert!(out.contains("kbf_pack_store_wait_seconds_count{op=\"put\"} 0\n"));
    assert!(out.contains("kbf_pack_store_retries_total{op=\"get\",cause=\"unavailable\"} 2\n"));
    assert!(out.contains("kbf_pack_store_retries_total{op=\"get\",cause=\"connection\"} 0\n"));
    assert!(out.contains("kbf_pack_store_retries_total{op=\"put\",cause=\"unavailable\"} 0\n"));
    assert!(out.ends_with("kbf_pack_input_walks_cut_total 1\n"));
    assert_eq!(waits.walks_cut(), 1);
}

/// Catches a store down for good that costs every call the whole budget (a pack written again
/// under another key, the delete of the failed key, each read of a preparation): once a call has
/// spent its budget, a later call's first failure is its answer at once, with no wait, saying why;
/// a request that got no answer leaves the store taken as down; an answer of the store takes it
/// as up; and a 503 after that is waited for again.
#[tokio::test]
async fn a_store_down_for_good_costs_the_budget_once() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, STORE_WAIT, unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, STORE_WAIT, &waits);
    let range = ByteRange::new(0, 1).expect("a range");
    store
        .inner
        .put_new(&key("held"), Bytes::from_static(b"h"), None)
        .await
        .expect("seeded");

    assert!(over.get_range(&key("held"), range).await.is_err());
    assert!(waits.down());
    let calls = store.calls();
    let now = clock.now();

    let at_once = over.delete(&key("held")).await.expect_err("refused");

    assert_eq!(
        at_once.to_string(),
        "store answered 503 ServiceUnavailable: injected (not retried: a call to the store spent \
         its budget of 120 s still failing, and no call since was answered)"
    );
    assert_eq!(store.calls(), calls + 1);
    assert_eq!(clock.now(), now);
    assert_eq!(waits.exhausted(Op::Get), 1);
    assert_eq!(waits.exhausted(Op::Delete), 1);
    assert_eq!(waits.waits(Op::Delete).count(), 0);

    let dead = Failing::new(&clock, Duration::MAX, reset);
    let unanswered = waiting(&dead, &clock, STORE_WAIT, &waits);
    let said = unanswered.len(&key("held")).await.expect_err("failed");
    assert!(
        said.to_string().contains("not retried"),
        "a request with no answer was waited for while the store was taken as down: {said}"
    );
    assert!(
        waits.down(),
        "a request with no answer took the store as up"
    );

    clock.pass(Duration::from_secs(1));
    assert_eq!(over.len(&key("held")).await.expect("asked"), Some(1));
    assert!(!waits.down());

    let again = Failing::new(&clock, Duration::MAX, unavailable);
    let over_again = waiting(&again, &clock, Duration::from_secs(1), &waits);
    assert!(
        over_again
            .list(
                &KeyPrefix::new("cas/").expect("a prefix"),
                None,
                PageSize::MAX
            )
            .await
            .is_err()
    );
    assert_eq!(
        waits.waits(Op::List).count(),
        1,
        "a 503 after the store was up was not waited for"
    );
}

/// Catches a missing object that leaves a store taken as down: an answer that is not retried
/// (here a missing object) takes the store as up.
#[tokio::test]
async fn any_answer_of_the_store_takes_it_as_up() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::ZERO, &waits);
    assert!(over.len(&key("x")).await.is_err());
    assert!(waits.down());

    let up = Failing::new(&clock, Duration::ZERO, unavailable);
    let over = waiting(&up, &clock, Duration::ZERO, &waits);
    let range = ByteRange::new(0, 1).expect("a range");
    let missing = over.get_range(&key("none"), range).await;

    assert!(
        matches!(missing, Err(ObjectStoreError::NotFound(_))),
        "{missing:?}"
    );
    assert!(!waits.down());
}

/// Catches a call already being retried cut short when another call takes the store as down: a
/// call whose first failure was waited for is still retried after the mark is set, within its own
/// budget, and is not counted as exhausted; only a call's first failure is answered at once.
#[tokio::test]
async fn a_call_already_retrying_keeps_its_budget_when_the_store_is_taken_as_down() {
    let clock = Arc::new(Clock::default());
    let counted = Arc::new(StoreWaits::default());
    let backoff = Backoff::with(
        policy_without_jitter(Duration::from_secs(120)),
        Arc::clone(&clock) as Arc<dyn RetryClock>,
        Arc::clone(&counted),
    );
    let mut call = backoff.call(Op::Get);

    assert_eq!(call.after(&unavailable()).await, Next::Again);
    counted.down.store(true, Ordering::Relaxed);
    assert_eq!(call.after(&unavailable()).await, Next::Again);

    assert_eq!(counted.exhausted(Op::Get), 0);
    assert_eq!(clock.waits(), [secs(0.5), secs(1.0)]);
    assert_eq!(
        format!("{backoff:?}"),
        format!(
            "Backoff {{ policy: {:?}, .. }}",
            policy_without_jitter(Duration::from_secs(120))
        )
    );
}

/// Catches an answer given for good after retries of others dressed as one that kept failing:
/// a read the store answers 503 twice and then 404 (no such object) is the store's 404 as it gave
/// it, with no word of the attempts, after the two retries counted under their cause.
#[tokio::test]
async fn an_answer_given_for_good_after_retries_is_the_store_s_as_it_is() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, secs(1.5), unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::from_secs(120), &waits);

    let range = ByteRange::new(0, 1).expect("a range");
    let error = over
        .get_range(&key("absent"), range)
        .await
        .expect_err("absent");

    assert!(matches!(error, ObjectStoreError::NotFound(_)), "{error:?}");
    let as_given = store.inner.get_range(&key("absent"), range).await;
    assert_eq!(error.to_string(), as_given.expect_err("absent").to_string());
    assert_eq!(waits.retries(Op::Get, Cause::Unavailable), 2);
    assert_eq!(waits.exhausted(Op::Get), 0);
}

/// Catches a refusal after retries of others dressed as a failure that kept on: a call the store
/// answered 503 and then 403 is the store's 403 as it gave it, with no word of the attempts.
#[tokio::test]
async fn a_refusal_after_retries_is_the_store_s_as_it_is() {
    let clock = Arc::new(Clock::default());
    let backoff = Backoff::with(
        policy_without_jitter(Duration::from_secs(120)),
        Arc::clone(&clock) as Arc<dyn RetryClock>,
        Arc::default(),
    );
    let mut call = backoff.call(Op::Put);
    let forbidden = || ObjectStoreError::Service {
        status: 403,
        code: "AccessDenied".to_owned(),
        message: "injected".to_owned(),
    };

    assert_eq!(call.after(&unavailable()).await, Next::Again);
    assert_eq!(call.after(&forbidden()).await, Next::Answer);
    let answer = call.exhausted(forbidden());

    assert_eq!(answer.to_string(), forbidden().to_string());
    assert!(
        matches!(answer, ObjectStoreError::Service { status: 403, .. }),
        "{answer:?}"
    );
}

/// A transport failure with a cause of its own, as the store client gives one (the request's
/// error, then the socket's).
#[derive(Debug)]
struct Outer(std::io::Error);

impl std::fmt::Display for Outer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the request failed")
    }
}

impl Error for Outer {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

fn reset_with_cause() -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(Outer(std::io::Error::other("connection reset"))))
}

/// Catches the causes of a transport failure lost once it was retried: the answer of a call that
/// kept failing so says each cause of the failure, in order, then the attempts.
#[tokio::test]
async fn a_retried_transport_failure_says_each_of_its_causes() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, reset_with_cause);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::from_secs(1), &waits);

    let error = over.len(&key("gone")).await.expect_err("failed");

    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "request to the store failed: the request failed: connection reset (still failing after \
         2 attempts over 0.5 s, the budget being 1 s)"
    );
}

/// Where the log of [`each_retry_is_logged_with_its_call_its_cause_and_its_wait`] goes.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("not poisoned")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches a retry that leaves no trace of why a call was slow: each retry is logged at DEBUG
/// with the call's op, the cause of the failure, the attempt it follows and the wait.
#[tokio::test]
async fn each_retry_is_logged_with_its_call_its_cause_and_its_wait() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, secs(0.5), unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::from_secs(120), &waits);

    over.len(&key("k")).await.expect("answered");

    let text = String::from_utf8(log.0.lock().expect("not poisoned").clone()).expect("UTF-8");
    let retries: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("a call to the pack store failed; trying it again"))
        .collect();
    assert_eq!(retries.len(), 1, "{text}");
    let line = retries[0];
    assert!(line.starts_with("DEBUG "), "{line}");
    for field in [
        "op=\"head\"",
        "cause=\"unavailable\"",
        "attempt=1",
        "wait=500ms",
        "error=store answered 503 ServiceUnavailable: injected",
    ] {
        assert!(line.contains(field), "{field}: {line}");
    }
}
