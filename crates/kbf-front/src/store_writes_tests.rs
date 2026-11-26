//! [`StoreWrites`]: the writes share the slots, a tree's manifests, roots and lists take slots of
//! their own, a `SlowDown` of a write whose body is in hand is waited out holding the slot, and a
//! streamed write is made once. (The waits for a store's
//! other 503s are tested with [`Waiting`](crate::store_wait::Waiting), in `store_wait_tests`.)
//! The waits are recorded by a fake [`Sleeper`] (or held by one until a test lets them end),
//! never taken, and every bound is checked by counting the calls the store saw.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use futures::stream;
use kbf_objstore::MemoryStore;
use tokio::sync::{Semaphore, oneshot, watch};

use super::*;
use crate::action_tree::{FIRST_WAIT, Sleeper};
use crate::store_wait::{STORE_WAIT, no_jitter};
use std::future::Future;
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

/// `n` slots over `fake`, and as many for tree writes, waiting with `sleeps`.
fn over(fake: &Arc<Fake>, n: usize, sleeps: &Arc<Sleeps>) -> Arc<StoreWrites<Fake>> {
    over_with(fake, n, n, sleeps)
}

/// `n` slots over `fake` and `trees` slots for tree writes, waiting with `sleeps`.
fn over_with(
    fake: &Arc<Fake>,
    n: usize,
    trees: usize,
    sleeps: &Arc<Sleeps>,
) -> Arc<StoreWrites<Fake>> {
    let sleeper = Arc::clone(sleeps) as Arc<dyn Sleeper>;
    let backoff = Backoff::with(STORE_WAIT, sleeper, no_jitter(), Arc::default());
    Arc::new(StoreWrites::new(
        Arc::clone(fake),
        slots(n),
        slots(trees),
        backoff,
    ))
}

/// A one-byte body that comes only once `release` is sent or dropped: a streamed write of it holds
/// its slot inside the store until then.
fn held_body(release: oneshot::Receiver<()>) -> BodyStream {
    Box::pin(stream::once(async move {
        let _ = release.await;
        Ok(Bytes::from_static(b"h"))
    }))
}

/// `write` made as a tree's write (under a tally), answered with where its time went.
async fn as_tree<T>(write: impl Future<Output = T>) -> (T, crate::action_tree::WriteWaits) {
    crate::action_tree::tallied(Duration::ZERO, write).await
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

/// Catches a default bound that is not the flags' documented 32 tree slots, or one that is not the
/// tree writer's manifest slots (a manifest with one of those should wait for no other).
#[test]
fn the_tree_writes_have_32_slots_as_many_as_the_manifest_slots() {
    assert_eq!(TREE_WRITES.get(), 32);
    assert_eq!(TREE_WRITES.get(), crate::action_tree::MANIFEST_WRITES);
}

/// Catches a tree's write that queues behind the uploads for a slot, the failure the tree slots
/// exist for, streamed or whole: with the one upload slot held by a streamed upload whose body has
/// not come, a tree's write reaches the store and is stored with no wait for a slot, while the
/// upload still holds its slot; the upload is stored once its body comes.
#[tokio::test]
async fn a_tree_write_reaches_the_store_while_every_upload_slot_is_held() {
    for streamed in [true, false] {
        let fake = Arc::new(Fake::open());
        let store = over_with(&fake, 1, 1, &Sleeps::at_once());
        let (release, released) = oneshot::channel();
        let upload = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                store
                    .put_stream(&key("upload"), 1, held_body(released))
                    .await
            }
        });
        fake.until_inside(1).await;
        let tree_key = key("tree");
        let write = async {
            match streamed {
                true => store.put_stream(&tree_key, 3, body(b"def")).await,
                false => {
                    store
                        .put_new(&tree_key, Bytes::from_static(b"def"), None)
                        .await
                }
            }
        };

        let (put, waits) = as_tree(write).await;

        put.expect("stored");
        assert_eq!(waits.store_slot, Duration::ZERO, "streamed: {streamed}");
        assert_eq!(store.len(&tree_key).await.expect("asked"), Some(3));
        assert!(!upload.is_finished(), "the upload still holds its slot");
        release.send(()).expect("the upload waits for its body");
        upload.await.expect("joined").expect("stored");
        assert_eq!(fake.writes(), 2);
    }
}

/// Catches an upload that queues behind the trees' writes, or a tree slot taken by a write that
/// is not a tree's: with the one tree slot held by a tree's write whose body has not come, an
/// upload reaches the store and is stored.
#[tokio::test]
async fn an_upload_reaches_the_store_while_every_tree_slot_is_held() {
    let fake = Arc::new(Fake::open());
    let store = over_with(&fake, 1, 1, &Sleeps::at_once());
    let (release, released) = oneshot::channel();
    let tree = tokio::spawn({
        let store = Arc::clone(&store);
        as_tree(async move { store.put_stream(&key("tree"), 1, held_body(released)).await })
    });
    fake.until_inside(1).await;

    store
        .put_new(&key("upload"), Bytes::from_static(b"u"), None)
        .await
        .expect("stored");

    assert!(!tree.is_finished(), "the tree's write still holds its slot");
    release
        .send(())
        .expect("the tree's write waits for its body");
    let (put, _) = tree.await.expect("joined");
    put.expect("stored");
    assert_eq!(fake.writes(), 2);
}

/// Catches tree writes past their own bound reaching the store, or the two bounds merged into
/// one: with one upload slot, one tree slot and the store holding every write it gets, three
/// uploads and three tree writes (whole and streamed) put exactly two inside it, one of each; once
/// the store lets them through, all six are stored and no more than two were ever inside at once.
#[tokio::test]
async fn uploads_and_tree_writes_each_stay_within_their_slots() {
    let fake = Arc::new(Fake::with_gate(0));
    let store = over_with(&fake, 1, 1, &Sleeps::at_once());
    let mut writes = Vec::new();
    for n in 0..6 {
        let store = Arc::clone(&store);
        let write = async move {
            let key = key(&format!("w{n}"));
            match n % 2 == 0 {
                true => store.put_new(&key, Bytes::from_static(b"w"), None).await,
                false => store.put_stream(&key, 1, body(b"w")).await,
            }
        };
        writes.push(tokio::spawn(async move {
            match n < 3 {
                true => write.await,
                false => as_tree(write).await.0,
            }
        }));
    }

    fake.until_inside(2).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fake.writes(), 2, "a write past its slots reached the store");
    fake.gate.add_permits(6);
    for write in writes {
        write.await.expect("ran").expect("stored");
    }

    assert_eq!(fake.writes(), 6);
    assert_eq!(fake.most(), 2);
}

/// Catches the wait for a tree slot left out of a tree's write, told to a write that did not
/// wait, or told as its request, for a streamed write and a whole one alike: with the one tree
/// slot held by another tree's write, a tree's write waits for it; 7 ms on tokio's paused clock
/// later the held write is let through, and the waiting write is told its 7 ms as its store slot,
/// with no 503 wait and no request time (the store answers at once), while the held write, which
/// waited for nothing, is told no store slot.
#[tokio::test(start_paused = true)]
async fn a_tree_write_is_told_its_wait_for_a_tree_slot_and_no_other_write_is() {
    for streamed in [true, false] {
        let fake = Arc::new(Fake::with_gate(0));
        let store = over_with(&fake, 1, 1, &Sleeps::at_once());
        let held = tokio::spawn({
            let store = Arc::clone(&store);
            as_tree(async move { store.put_stream(&key("held"), 3, body(b"abc")).await })
        });
        fake.until_inside(1).await;
        let tree_key = key("tree");
        let write = async {
            match streamed {
                true => store.put_stream(&tree_key, 3, body(b"def")).await,
                false => {
                    store
                        .put_new(&tree_key, Bytes::from_static(b"def"), None)
                        .await
                }
            }
        };
        let mut tree = std::pin::pin!(as_tree(write));
        assert!(
            futures::poll!(tree.as_mut()).is_pending(),
            "no tree slot is free"
        );
        assert_eq!(fake.writes(), 1, "the tree's write waits outside the store");
        tokio::time::advance(Duration::from_millis(7)).await;
        fake.gate.add_permits(2);
        let (put, held_waits) = held.await.expect("joined");
        put.expect("stored");
        assert_eq!(held_waits.store_slot, Duration::ZERO);
        let (put, waits) = tree.await;
        put.expect("stored");
        let want = crate::action_tree::WriteWaits {
            manifest_slot: Duration::ZERO,
            store_slot: Duration::from_millis(7),
            store_503: Duration::ZERO,
            request: Duration::ZERO,
        };
        assert_eq!(waits, want, "streamed: {streamed}");
    }
}
