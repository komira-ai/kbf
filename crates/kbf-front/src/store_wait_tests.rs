//! [`Waiting`] and [`Backoff`]: every call to a store that answers 503 for a while is made again
//! after jittered waits until the store answers, within [`STORE_WAIT`]; a store that still
//! answers 503 when the waits reach the bound is the call's answer, saying so; a `SlowDown` of a
//! whole write is waited out past the bound; every wait is counted. Time is a virtual clock that
//! the fake sleeper moves by each wait it is asked for, so a window of 30 s is 30 s of waits,
//! counted, never taken.

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use futures::future::BoxFuture;
use futures::stream;
use kbf_objstore::MemoryStore;

use super::*;

/// The virtual clock: the waits taken so far, added up.
#[derive(Default)]
struct Clock {
    now: Mutex<Duration>,
    waits: Mutex<Vec<Duration>>,
}

impl Clock {
    fn now(&self) -> Duration {
        *self.now.lock().expect("not poisoned")
    }

    fn waits(&self) -> Vec<Duration> {
        self.waits.lock().expect("not poisoned").clone()
    }
}

impl Sleeper for Clock {
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        *self.now.lock().expect("not poisoned") += wait;
        self.waits.lock().expect("not poisoned").push(wait);
        Box::pin(std::future::ready(()))
    }
}

/// A memory store that answers every call `answer` until the clock reaches `until`, and counts
/// the calls it was asked.
struct Failing {
    inner: MemoryStore,
    clock: Arc<Clock>,
    until: Duration,
    answer: fn() -> ObjectStoreError,
    calls: AtomicUsize,
}

impl Failing {
    fn new(clock: &Arc<Clock>, until: Duration, answer: fn() -> ObjectStoreError) -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            clock: Arc::clone(clock),
            until,
            answer,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The scripted answer while the store is failing, else `None`.
    fn refused(&self) -> Option<ObjectStoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.clock.now() < self.until).then(self.answer)
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

/// `store`, waiting on `clock` with no jitter for at most `total`, counted in `waits`.
fn waiting(
    store: &Arc<Failing>,
    clock: &Arc<Clock>,
    total: Duration,
    waits: &Arc<StoreWaits>,
) -> Waiting<Failing> {
    let sleeper = Arc::clone(clock) as Arc<dyn Sleeper>;
    let backoff = Backoff::with(total, sleeper, no_jitter(), Arc::default());
    Waiting::new(Arc::clone(store), backoff.counted_in(Arc::clone(waits)))
}

/// The nominal waits of a call to a store that fails over for 30 s: 0.5, 1, 2, 4, 8, 8 and 8 s,
/// which pass 30 s at the seventh (31.5 s).
const RIDDEN: [f64; 7] = [0.5, 1.0, 2.0, 4.0, 8.0, 8.0, 8.0];

/// Catches a call the store answers 503 during a failover of 30 s that fails instead of waiting:
/// each of a write, a ranged read, a length, a presence check, a delete and a listing, made when
/// the failover begins, is made again after the doubling waits until the store answers, and is
/// answered as the store answers once it is back; every wait is counted by its op.
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
    }
}

/// Catches a call that waits without end for a store that stays down, or ends without saying
/// why: a read of a store that answers 503 past the bound is made 12 times, after waits that add
/// up to exactly 60 s (the last cut from 8 s to 4.5 s), and its answer is the store's 503, its
/// code kept, saying how long it waited; the call is counted as exhausted.
#[tokio::test]
async fn a_store_down_past_the_bound_is_its_503_saying_so() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, STORE_WAIT, &waits);
    let range = ByteRange::new(0, 1).expect("a range");

    let answer = over.get_range(&key("gone"), range).await;

    let error = answer.expect_err("refused");
    assert_eq!(
        error.to_string(),
        "store answered 503 ServiceUnavailable: injected (still so after 12 attempts and 60.0 s \
         of waits)"
    );
    assert!(
        matches!(&error, ObjectStoreError::Service { status: 503, code, .. } if code == "ServiceUnavailable"),
        "{error:?}"
    );
    let nominal = [0.5, 1.0, 2.0, 4.0, 8.0, 8.0, 8.0, 8.0, 8.0, 8.0, 4.5].map(secs);
    assert_eq!(clock.waits(), nominal);
    assert_eq!(clock.now(), STORE_WAIT);
    assert_eq!(store.calls(), 12);
    assert_eq!(waits.exhausted(Op::Get), 1);
    assert_eq!(waits.waits(Op::Get).count(), 11);
}

/// Catches a bound of zero that still waits, or a 503 rewritten when nothing was waited for: with
/// no time to wait, a 503 is the call's answer at once, as the store gave it.
#[tokio::test]
async fn a_bound_of_zero_answers_the_first_503() {
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
}

/// Catches a `SlowDown` of a whole write that is bounded like a fault, or one of any other call
/// that is not: with a bound of 1 s, a write answered `SlowDown` for 20 s of waits is still
/// made and stored, while a read answered `SlowDown` is answered after waits of 0.5 and 0.5 s.
#[tokio::test]
async fn a_slow_down_of_a_whole_write_is_waited_out_past_the_bound() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::from_secs(20), slow_down);
    let waits = Arc::new(StoreWaits::default());
    let over = waiting(&store, &clock, Duration::from_secs(1), &waits);

    over.put_new(&key("w"), Bytes::from_static(b"w"), None)
        .await
        .expect("stored");

    let nominal = [0.5, 1.0, 2.0, 4.0, 8.0, 8.0].map(secs);
    assert_eq!(clock.waits(), nominal);
    assert_eq!(waits.exhausted(Op::Put), 0);

    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, slow_down);
    let over = waiting(&store, &clock, Duration::from_secs(1), &waits);
    let range = ByteRange::new(0, 1).expect("a range");
    let read = over.get_range(&key("w"), range).await;
    assert!(read.is_err());
    assert_eq!(clock.waits(), [0.5, 0.5].map(secs));
    assert_eq!(waits.exhausted(Op::Get), 1);
}

/// Catches an answer other than a 503 that is waited for: a 500, a missing object, a dead
/// connection and a refused replacement are each the call's answer on its first attempt, with
/// no wait.
#[tokio::test]
async fn every_answer_but_a_503_is_the_call_s_at_once() {
    fn dead() -> ObjectStoreError {
        ObjectStoreError::Transport(Arc::new(std::io::Error::other("broken pipe")))
    }
    for answer in [internal as fn() -> ObjectStoreError, dead] {
        let clock = Arc::new(Clock::default());
        let store = Failing::new(&clock, Duration::MAX, answer);
        let waits = Arc::new(StoreWaits::default());
        let over = waiting(&store, &clock, STORE_WAIT, &waits);

        let said = over.delete(&key("x")).await.expect_err("refused");

        assert_eq!(said.to_string(), answer().to_string());
        assert_eq!(store.calls(), 1);
        assert!(clock.waits().is_empty());
        assert_eq!(waits.exhausted(Op::Delete), 0);
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
    assert_eq!(format!("{over:?}"), "Waiting { total: 60s, .. }");
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
    assert_eq!(half_to_whole(nominal, 0), nominal / 2);
    assert_eq!(half_to_whole(nominal, 4_000_000_000), nominal);
    assert_eq!(half_to_whole(nominal, 4_000_000_001), nominal / 2);
    assert_eq!(half_to_whole(Duration::ZERO, u64::MAX), Duration::ZERO);
    assert_eq!(no_jitter()(nominal), nominal);
}

/// Catches jitter that lets a call wait past its bound: with the waits jittered, the calls to a
/// store that stays down wait at most the bound, exactly, and each wait is at least half of its
/// nominal one.
#[tokio::test]
async fn jittered_waits_still_end_at_the_bound() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, Duration::MAX, unavailable);
    let sleeper = Arc::clone(&clock) as Arc<dyn Sleeper>;
    let over = Waiting::new(
        Arc::clone(&store),
        Backoff::with(STORE_WAIT, sleeper, spread(), Arc::default()),
    );

    let prefix = KeyPrefix::new("cas/").expect("a prefix");

    let answer = over.list(&prefix, None, PageSize::MAX).await;

    assert!(answer.is_err());
    assert_eq!(clock.now(), STORE_WAIT);
    assert!(clock.waits()[0] >= FIRST_WAIT / 2);
}

/// Catches a wait family that is missing from the scrape or counts the wrong op: with no waits
/// both families render every op with zeros, and a wait of a read is counted under `op="get"`.
#[tokio::test]
async fn the_waits_render_by_op() {
    let waits = Arc::new(StoreWaits::default());
    let mut zero = String::new();
    waits.render(&mut zero);
    assert!(zero.starts_with(
        "# HELP kbf_pack_store_wait_seconds Waits for the packs' object store after it answered \
         503, by op\n# TYPE kbf_pack_store_wait_seconds histogram\n"
    ));
    assert!(zero.contains("kbf_pack_store_wait_seconds_count{op=\"list\"} 0\n"));
    assert!(zero.ends_with(
        "# TYPE kbf_pack_store_waits_exhausted_total counter\n\
         kbf_pack_store_waits_exhausted_total{op=\"get\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"head\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"put\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"delete\"} 0\n\
         kbf_pack_store_waits_exhausted_total{op=\"list\"} 0\n"
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

    let mut out = String::new();
    waits.render(&mut out);
    assert!(out.contains("kbf_pack_store_wait_seconds_count{op=\"get\"} 2\n"));
    assert!(out.contains("kbf_pack_store_wait_seconds_sum{op=\"get\"} 1.5\n"));
    assert!(out.contains("kbf_pack_store_wait_seconds_count{op=\"put\"} 0\n"));
}

/// Catches a store down for good that costs every call the whole bound (a pack written again
/// under another key, the delete of the failed key, each read of a preparation): once a call has
/// waited out the bound, a later call's 503 is its answer at once, with no wait, saying why; a
/// request that got no answer leaves the store taken as down; an answer of the store takes it as
/// up; and a 503 after that is waited for again.
#[tokio::test]
async fn a_store_down_for_good_costs_the_bound_once() {
    let clock = Arc::new(Clock::default());
    let store = Failing::new(&clock, STORE_WAIT + Duration::from_secs(1), unavailable);
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

    let at_once = over.delete(&key("held")).await.expect_err("refused");

    assert_eq!(
        at_once.to_string(),
        "store answered 503 ServiceUnavailable: injected (not waited for: the store answered 503 \
         past the bound of 60 s of waits, and no call since)"
    );
    assert_eq!(store.calls(), calls + 1);
    assert_eq!(clock.now(), STORE_WAIT);
    assert_eq!(waits.exhausted(Op::Get), 1);
    assert_eq!(waits.exhausted(Op::Delete), 1);
    assert_eq!(waits.waits(Op::Delete).count(), 0);

    let dead = Failing::new(&clock, Duration::MAX, || {
        ObjectStoreError::Transport(Arc::new(std::io::Error::other("reset")))
    });
    let unanswered = waiting(&dead, &clock, STORE_WAIT, &waits);
    assert!(unanswered.len(&key("held")).await.is_err());
    assert!(
        waits.down(),
        "a request with no answer took the store as up"
    );

    clock.sleep(Duration::from_secs(1)).await;
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
        2,
        "a 503 after the store was up was not waited for"
    );
}

/// Catches a missing object that leaves a store taken as down: an answer that is not a 503 (here
/// a missing object) takes the store as up.
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
