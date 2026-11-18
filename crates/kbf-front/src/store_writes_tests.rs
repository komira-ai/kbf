//! [`StoreWrites`]: the writes share the slots, a `SlowDown` of a write whose body is in hand is
//! waited out holding the slot, and a streamed write is made once. (The waits for a store's
//! other 503s are tested with [`Waiting`](crate::store_wait::Waiting), in `store_wait_tests`.)
//! The waits are recorded by a fake [`Sleeper`] (or held by one until a test lets them end),
//! never taken, and every bound is checked by counting the calls the store saw.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use futures::stream;
use kbf_objstore::MemoryStore;
use tokio::sync::{Semaphore, watch};

use super::*;
use crate::action_tree::{FIRST_WAIT, Sleeper};
use crate::store_wait::{STORE_WAIT, no_jitter};
use std::time::Duration;

/// A memory store whose writes answer the errors scripted for them first, wait at a gate the
/// test opens, and count how many are inside at once.
struct Fake {
    inner: MemoryStore,
    script: Mutex<VecDeque<ObjectStoreError>>,
    gate: Semaphore,
    inside: watch::Sender<usize>,
    most: AtomicUsize,
    writes: AtomicUsize,
}

impl Fake {
    /// A store whose gate is open.
    fn open() -> Self {
        Self::with_gate(Semaphore::MAX_PERMITS)
    }

    /// A store whose gate lets `passes` writes through until the test adds more.
    fn with_gate(passes: usize) -> Self {
        Self {
            inner: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            script: Mutex::new(VecDeque::new()),
            gate: Semaphore::new(passes),
            inside: watch::Sender::new(0),
            most: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        }
    }

    /// The next writes answer `errors`, in order, before any is made.
    fn answer(&self, errors: impl IntoIterator<Item = ObjectStoreError>) {
        self.lock().extend(errors);
    }

    /// The writes the store has been asked to make.
    fn writes(&self) -> usize {
        self.writes.load(SeqCst)
    }

    /// The most writes that were inside the store at once.
    fn most(&self) -> usize {
        self.most.load(SeqCst)
    }

    /// Waits until at least `n` writes are inside the store.
    async fn until_inside(&self, n: usize) {
        let mut inside = self.inside.subscribe();
        inside
            .wait_for(|now| *now >= n)
            .await
            .expect("the store lives");
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<ObjectStoreError>> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a write in, waits at the gate, and answers the next scripted error, if any.
    async fn enter(&self) -> Option<ObjectStoreError> {
        self.writes.fetch_add(1, SeqCst);
        self.inside.send_modify(|n| *n += 1);
        self.most.fetch_max(*self.inside.borrow(), SeqCst);
        self.gate.acquire().await.expect("never closed").forget();
        self.lock().pop_front()
    }

    fn leave(&self) {
        self.inside.send_modify(|n| *n -= 1);
    }
}

impl ObjectStore for Fake {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let answer = match self.enter().await {
            Some(error) => Err(error),
            None => self.inner.put_new(key, body, retain_until).await,
        };
        self.leave();
        answer
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        let answer = match self.enter().await {
            Some(error) => Err(error),
            None => self.inner.put_stream(key, len, body).await,
        };
        self.leave();
        answer
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        self.inner.len(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A [`Sleeper`] that records each wait, and returns at once or, when held, once the test lets
/// one wait end.
struct Sleeps {
    waits: watch::Sender<Vec<Duration>>,
    ends: Semaphore,
}

impl Sleeps {
    /// Waits that end at once.
    fn at_once() -> Arc<Self> {
        Self::with_ends(Semaphore::MAX_PERMITS)
    }

    /// Waits that end only as the test lets them ([`Sleeps::end_one`]).
    fn held() -> Arc<Self> {
        Self::with_ends(0)
    }

    fn with_ends(ends: usize) -> Arc<Self> {
        Arc::new(Self {
            waits: watch::Sender::new(Vec::new()),
            ends: Semaphore::new(ends),
        })
    }

    fn waits(&self) -> Vec<Duration> {
        self.waits.borrow().clone()
    }

    fn end_one(&self) {
        self.ends.add_permits(1);
    }

    /// Waits until `n` waits have begun.
    async fn until_begun(&self, n: usize) {
        let mut waits = self.waits.subscribe();
        waits
            .wait_for(|w| w.len() == n)
            .await
            .expect("the sleeper lives");
    }
}

impl Sleeper for Sleeps {
    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        self.waits.send_modify(|w| w.push(wait));
        Box::pin(async move {
            self.ends.acquire().await.expect("never closed").forget();
        })
    }
}

fn slots(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("not zero")
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/test/{name}")).expect("a key")
}

fn service(status: u16, code: &str) -> ObjectStoreError {
    ObjectStoreError::Service {
        status,
        code: code.to_owned(),
        message: "injected".to_owned(),
    }
}

/// The store's back-pressure, as tierstore answers it.
fn slow_down() -> ObjectStoreError {
    service(503, "SlowDown")
}

fn body(bytes: &'static [u8]) -> BodyStream {
    Box::pin(stream::iter([Ok(Bytes::from_static(bytes))]))
}

/// `slots` over `fake`, waiting with `sleeps`.
fn over(fake: &Arc<Fake>, n: usize, sleeps: &Arc<Sleeps>) -> Arc<StoreWrites<Fake>> {
    let sleeper = Arc::clone(sleeps) as Arc<dyn Sleeper>;
    let backoff = Backoff::with(STORE_WAIT, sleeper, no_jitter(), Arc::default());
    Arc::new(StoreWrites::new(Arc::clone(fake), slots(n), backoff))
}

/// Catches a `SlowDown` that fails a write, or one waited out on a schedule other than the
/// tree writer's: a write answered `SlowDown` six times is made a seventh time and stored, after
/// waits of 0.5, 1, 2, 4, 8 and 8 seconds (doubling, then held at the longest wait).
#[tokio::test]
async fn a_slow_down_is_waited_out_with_doubling_waits() {
    let fake = Arc::new(Fake::open());
    let sleeps = Sleeps::at_once();
    let store = over(&fake, 4, &sleeps);
    fake.answer(std::iter::repeat_with(slow_down).take(6));

    let put = store
        .put_new(&key("a"), Bytes::from_static(b"abc"), None)
        .await;

    put.expect("stored");
    assert_eq!(fake.writes(), 7);
    let secs = |s: f64| Duration::from_secs_f64(s);
    let want = [0.5, 1.0, 2.0, 4.0, 8.0, 8.0].map(secs);
    assert_eq!(sleeps.waits(), want);
    let range = ByteRange::new(0, 3).expect("a range");
    let held = store.get_range(&key("a"), range).await;
    assert_eq!(held.expect("read"), Bytes::from_static(b"abc"));
}

/// Catches a writer that repeats or swallows an answer other than a 503: a 500, a 403, a dead
/// connection and a refusal to replace an object are each the write's answer on its first
/// attempt, with no wait.
#[tokio::test]
async fn every_other_answer_is_the_write_s_at_once() {
    let dead = || ObjectStoreError::Transport(Arc::new(std::io::Error::other("broken pipe")));
    let answers = [
        service(500, "InternalError"),
        service(403, "AccessDenied"),
        dead(),
    ];
    for (n, answer) in answers.into_iter().enumerate() {
        let fake = Arc::new(Fake::open());
        let sleeps = Sleeps::at_once();
        let store = over(&fake, 4, &sleeps);
        let said = answer.to_string();
        fake.answer([answer]);

        let put = store
            .put_new(&key("b"), Bytes::from_static(b"b"), None)
            .await;

        assert_eq!(put.expect_err("refused").to_string(), said, "answer {n}");
        assert_eq!(fake.writes(), 1, "{said}");
        assert!(sleeps.waits().is_empty(), "{said}");
    }
    let fake = Arc::new(Fake::open());
    let sleeps = Sleeps::at_once();
    let store = over(&fake, 4, &sleeps);
    store
        .put_new(&key("c"), Bytes::from_static(b"c"), None)
        .await
        .expect("stored");
    let again = store
        .put_new(&key("c"), Bytes::from_static(b"c"), None)
        .await;
    assert!(
        matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
        "{again:?}"
    );
    assert_eq!(fake.writes(), 2);
    assert!(sleeps.waits().is_empty());
}

/// Catches a streamed write made again after a `SlowDown`, which would send a body that was
/// already read: the `SlowDown` is the write's answer, after one attempt and no wait, and a
/// streamed write the store takes is stored.
#[tokio::test]
async fn a_streamed_write_is_made_once() {
    let fake = Arc::new(Fake::open());
    let sleeps = Sleeps::at_once();
    let store = over(&fake, 4, &sleeps);
    fake.answer([slow_down()]);

    let refused = store.put_stream(&key("d"), 1, body(b"d")).await;

    let said = refused.expect_err("refused").to_string();
    assert_eq!(said, slow_down().to_string());
    assert_eq!(fake.writes(), 1);
    assert!(sleeps.waits().is_empty());
    store
        .put_stream(&key("d"), 1, body(b"d"))
        .await
        .expect("stored");
    assert_eq!(store.len(&key("d")).await.expect("asked"), Some(1));
}

/// Catches writes past the bound reaching the store: with two slots and the store holding every
/// write it gets, six writes (three whole, three streamed) put two inside it and no more; once
/// the store lets them through, all six are stored and no more than two were ever inside at once.
#[tokio::test]
async fn writes_never_exceed_the_slots() {
    let fake = Arc::new(Fake::with_gate(0));
    let store = over(&fake, 2, &Sleeps::at_once());
    let mut writes = Vec::new();
    for n in 0..6 {
        let store = Arc::clone(&store);
        writes.push(tokio::spawn(async move {
            let key = key(&format!("w{n}"));
            if n % 2 == 0 {
                store.put_new(&key, Bytes::from_static(b"w"), None).await
            } else {
                store.put_stream(&key, 1, body(b"w")).await
            }
        }));
    }

    fake.until_inside(2).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fake.writes(), 2, "a write past the slots reached the store");
    fake.gate.add_permits(6);
    for write in writes {
        write.await.expect("ran").expect("stored");
    }

    assert_eq!(fake.writes(), 6);
    assert_eq!(fake.most(), 2);
}

/// Catches a slot given back while its write waits out a `SlowDown`, which would let another
/// write onto a store that asked for fewer: with one slot, a second write does not reach the
/// store while the first waits, and does once the first is stored.
#[tokio::test]
async fn a_write_holds_its_slot_through_its_waits() {
    let fake = Arc::new(Fake::open());
    let sleeps = Sleeps::held();
    let store = over(&fake, 1, &sleeps);
    fake.answer([slow_down()]);
    let first = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .put_new(&key("e"), Bytes::from_static(b"e"), None)
                .await
        })
    };
    sleeps.until_begun(1).await;
    let second = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .put_new(&key("f"), Bytes::from_static(b"f"), None)
                .await
        })
    };

    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fake.writes(),
        1,
        "the second write reached the store during the wait"
    );
    sleeps.end_one();
    first.await.expect("ran").expect("stored");
    second.await.expect("ran").expect("stored");

    assert_eq!(fake.writes(), 3);
    assert_eq!(sleeps.waits(), [FIRST_WAIT]);
}

/// Catches a read, a length, a delete or a listing that waits for a write's slot: with the only
/// slot held by a write the store is holding, each of them is answered.
#[tokio::test]
async fn reads_take_no_slot() {
    let fake = Arc::new(Fake::with_gate(0));
    let store = over(&fake, 1, &Sleeps::at_once());
    fake.inner
        .put_new(&key("g"), Bytes::from_static(b"held"), None)
        .await
        .expect("seeded");
    let stuck = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .put_new(&key("h"), Bytes::from_static(b"h"), None)
                .await
        })
    };
    fake.until_inside(1).await;

    let range = ByteRange::new(0, 4).expect("a range");
    let read = store.get_range(&key("g"), range).await.expect("read");
    assert_eq!(read, Bytes::from_static(b"held"));
    assert_eq!(store.len(&key("g")).await.expect("asked"), Some(4));
    assert!(store.may_hold(&key("g")).await.expect("asked"));
    let prefix = KeyPrefix::new("cas/test/").expect("a prefix");
    let listed = store
        .list(&prefix, None, PageSize::MAX)
        .await
        .expect("listed");
    assert_eq!(listed.objects.len(), 1);
    store.delete(&key("g")).await.expect("deleted");
    assert_eq!(store.len(&key("g")).await.expect("asked"), None);
    assert_eq!(store.capabilities(), fake.capabilities());

    fake.gate.add_permits(1);
    stuck.await.expect("ran").expect("stored");
}

/// Catches a default bound that is not the flag's documented 64.
#[test]
fn the_default_bound_is_64() {
    assert_eq!(STORE_WRITES.get(), 64);
}
