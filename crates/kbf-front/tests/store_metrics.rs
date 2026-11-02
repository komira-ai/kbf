//! The counting decorator on an object store (`kbf_front::store_metrics`) and the families it
//! feeds (`kbf_cold_store_*` on the cache's scrape, `kbf_pack_store_*` on the packs'). Time is a
//! stopwatch the test moves: a store takes a set amount of it per call, so a latency is placed
//! in its bucket by that amount and no test reads the wall.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::stream;
use kbf_front::store_metrics::{Counted, Op, Stopwatch, StoreMetrics};
use kbf_front::{Cache, PackSettings, Packs};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::PackIndex;

const COLD_ZERO: &str = include_str!("golden/cold_store_metrics_zero.txt");
const PACKS_ZERO: &str = include_str!("golden/pack_store_metrics_zero.txt");

/// The injected stopwatch, in nanoseconds.
#[derive(Clone, Default)]
struct Ticks(Arc<AtomicU64>);

impl Ticks {
    fn stopwatch(&self) -> Stopwatch {
        let ticks = Arc::clone(&self.0);
        Arc::new(move || Duration::from_nanos(ticks.load(Ordering::SeqCst)))
    }

    fn pass(&self, by: Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
    }
}

/// A store that takes `cost` of the stopwatch for every call, and keeps its objects in memory.
struct Slow {
    inner: MemoryStore,
    ticks: Ticks,
    cost: Duration,
}

impl ObjectStore for Slow {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.ticks.pass(self.cost);
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.ticks.pass(self.cost);
        self.inner.get_range(key, range).await
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        self.ticks.pass(self.cost);
        self.inner.len(key).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.ticks.pass(self.cost);
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.ticks.pass(self.cost);
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A store that answers every call with `error`.
struct Refuses(ObjectStoreError);

impl ObjectStore for Refuses {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn put_new(
        &self,
        _: &ObjectKey,
        _: Bytes,
        _: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        Err(self.0.clone())
    }

    async fn get_range(&self, _: &ObjectKey, _: ByteRange) -> Result<Bytes, ObjectStoreError> {
        Err(self.0.clone())
    }

    async fn len(&self, _: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        Err(self.0.clone())
    }

    async fn delete(&self, _: &ObjectKey) -> Result<(), ObjectStoreError> {
        Err(self.0.clone())
    }

    async fn list(
        &self,
        _: &KeyPrefix,
        _: Option<&ListToken>,
        _: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        Err(self.0.clone())
    }
}

/// A store that is sure it holds nothing, without a request.
struct NoHold(MemoryStore);

impl ObjectStore for NoHold {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.get_range(key, range).await
    }

    async fn may_hold(&self, _: &ObjectKey) -> Result<bool, ObjectStoreError> {
        Ok(false)
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a key")
}

fn range(len: u64) -> ByteRange {
    ByteRange::new(0, len).expect("a range")
}

fn page() -> PageSize {
    PageSize::new(10).expect("a page size")
}

fn body(len: usize) -> BodyStream {
    Box::pin(stream::once(async move { Ok(Bytes::from(vec![7; len])) }))
}

fn has(out: &str, line: &str) {
    assert!(out.lines().any(|l| l == line), "{line} is not in\n{out}");
}

fn memory() -> MemoryStore {
    MemoryStore::new(Capabilities::default())
}

fn scrape(metrics: &StoreMetrics) -> String {
    let mut out = String::new();
    metrics.render(&mut out);
    out
}

/// Catches: an op counted twice (a put counted by its stream and again by the whole body), an
/// op counted under another's label, a failed call that is not an error, and a call that
/// succeeds being one: two puts, one stream put, a read that finds the object and one that
/// does not, a length, a delete and a listing each count once under their own op, and a
/// store that refuses everything counts one error per op.
#[tokio::test]
async fn counted_store_counts_each_op_once_with_its_result() {
    let metrics = Arc::new(StoreMetrics::cold());
    let store = Counted::new(memory(), Arc::clone(&metrics));
    store
        .put_new(&key("a"), Bytes::from_static(b"x"), None)
        .await
        .expect("put");
    store
        .put_new(&key("b"), Bytes::from_static(b"y"), None)
        .await
        .expect("put");
    store
        .put_stream(&key("c"), 3, body(3))
        .await
        .expect("put stream");
    store.get_range(&key("a"), range(1)).await.expect("get");
    store
        .get_range(&key("missing"), range(1))
        .await
        .expect_err("no such object");
    store.len(&key("a")).await.expect("head");
    store.delete(&key("a")).await.expect("delete");
    store
        .list(&KeyPrefix::default(), None, page())
        .await
        .expect("list");
    let counts: Vec<_> = Op::ALL
        .map(|op| (metrics.requests(op), metrics.errors(op)))
        .to_vec();
    // get, head, put, delete, list
    assert_eq!(counts, [(2, 1), (1, 0), (3, 0), (1, 0), (1, 0)]);

    let refused = Arc::new(StoreMetrics::cold());
    let store = Counted::new(
        Refuses(ObjectStoreError::Protocol("no".to_owned())),
        Arc::clone(&refused),
    );
    store
        .put_new(&key("a"), Bytes::from_static(b"x"), None)
        .await
        .expect_err("refused");
    store
        .put_stream(&key("a"), 1, body(1))
        .await
        .expect_err("refused");
    store
        .get_range(&key("a"), range(1))
        .await
        .expect_err("refused");
    store.len(&key("a")).await.expect_err("refused");
    store.delete(&key("a")).await.expect_err("refused");
    store
        .list(&KeyPrefix::default(), None, page())
        .await
        .expect_err("refused");
    let counts: Vec<_> = Op::ALL
        .map(|op| (refused.requests(op), refused.errors(op)))
        .to_vec();
    assert_eq!(counts, [(1, 1), (1, 1), (2, 2), (1, 1), (1, 1)]);
}

/// Catches: bytes counted from what a read asked for rather than what the store returned,
/// bytes counted for a call that failed, and writes uncounted: a 10-byte object read with a
/// range of 1 MiB is 10 bytes read, a miss reads none, and a failed put writes none.
#[tokio::test]
async fn counted_store_counts_bytes_actually_read_not_asked() {
    let metrics = Arc::new(StoreMetrics::cold());
    let store = Counted::new(memory(), Arc::clone(&metrics));
    store
        .put_new(&key("a"), Bytes::from_static(b"0123456789"), None)
        .await
        .expect("put");
    store
        .put_stream(&key("b"), 6, body(6))
        .await
        .expect("put stream");
    assert_eq!(metrics.write_bytes(), 16);
    let got = store
        .get_range(&key("a"), range(1 << 20))
        .await
        .expect("get");
    assert_eq!(got.len(), 10);
    assert_eq!(metrics.read_bytes(), 10);
    store
        .get_range(&key("missing"), range(1 << 20))
        .await
        .expect_err("miss");
    assert_eq!(metrics.read_bytes(), 10);

    let refused = Arc::new(StoreMetrics::cold());
    let store = Counted::new(
        Refuses(ObjectStoreError::Body("cut".to_owned())),
        Arc::clone(&refused),
    );
    store
        .put_new(&key("a"), Bytes::from_static(b"0123456789"), None)
        .await
        .expect_err("refused");
    store
        .put_stream(&key("a"), 5, body(5))
        .await
        .expect_err("refused");
    assert_eq!(refused.write_bytes(), 0);
    let page = scrape(&metrics);
    has(&page, "kbf_cold_store_bytes_total{dir=\"read\"} 10");
    has(&page, "kbf_cold_store_bytes_total{dir=\"write\"} 16");
}

/// Catches: a call counted before the store is asked (a future built and dropped, never
/// polled, counts nothing), `may_hold` and `capabilities` counted as requests, and a
/// `may_hold` that is not passed to the store (a store that knows it has nothing is asked).
#[tokio::test]
async fn counted_store_adds_nothing_when_the_inner_call_is_not_made() {
    let metrics = Arc::new(StoreMetrics::cold());
    let store = Counted::new(NoHold(memory()), Arc::clone(&metrics));
    drop(store.get_range(&key("a"), range(1)));
    drop(store.len(&key("a")));
    drop(store.delete(&key("a")));
    drop(store.list(&KeyPrefix::default(), None, page()));
    drop(store.put_new(&key("a"), Bytes::new(), None));
    drop(store.put_stream(&key("a"), 0, body(0)));
    assert!(!store.may_hold(&key("a")).await.expect("answers"));
    assert_eq!(store.capabilities(), Capabilities::default());
    for op in Op::ALL {
        assert_eq!(metrics.requests(op), 0, "{op:?}");
        assert_eq!(metrics.seconds(op).count(), 0, "{op:?}");
    }
    assert_eq!(scrape(&metrics), scrape(&StoreMetrics::cold()));
}

/// Catches: an error changed on its way through (rewrapped, its key lost, a variant mapped to
/// another): every variant comes back from every op as it went in.
#[tokio::test]
async fn the_decorator_does_not_change_errors() {
    let errors = [
        ObjectStoreError::NotFound(key("k")),
        ObjectStoreError::AlreadyExists(key("k")),
        ObjectStoreError::InvalidRange {
            key: key("k"),
            range: range(4),
        },
        ObjectStoreError::Locked(key("k")),
        ObjectStoreError::Unsupported("object length"),
        ObjectStoreError::Service {
            status: 403,
            code: "AccessDenied".to_owned(),
            message: "no".to_owned(),
        },
        ObjectStoreError::Protocol("whole object".to_owned()),
        ObjectStoreError::Transport(Arc::new(io::Error::other("reset"))),
        ObjectStoreError::Body("short".to_owned()),
    ];
    for error in errors {
        let want = format!("{error:?}");
        let store = Counted::new(Refuses(error), Arc::new(StoreMetrics::cold()));
        let got = [
            store
                .put_new(&key("k"), Bytes::new(), None)
                .await
                .expect_err("put"),
            store
                .put_stream(&key("k"), 0, body(0))
                .await
                .expect_err("put stream"),
            store.get_range(&key("k"), range(1)).await.expect_err("get"),
            store.len(&key("k")).await.expect_err("head"),
            store.delete(&key("k")).await.expect_err("delete"),
            store
                .list(&KeyPrefix::default(), None, page())
                .await
                .expect_err("list"),
        ];
        for got in got {
            assert_eq!(format!("{got:?}"), want);
        }
    }
}

/// Catches: a latency observed in the wrong bucket, in the wrong op's histogram, observed twice
/// or not at all, and a sum that is not the time taken: one 3 ms read is one observation in the
/// 5 ms bucket (not the 2.5 ms one) of `get`, and none anywhere else.
#[tokio::test]
async fn latency_lands_in_the_bucket_of_the_time_the_stopwatch_says() {
    let ticks = Ticks::default();
    let slow = Slow {
        inner: memory(),
        ticks: ticks.clone(),
        cost: Duration::from_millis(3),
    };
    let metrics = Arc::new(StoreMetrics::cold());
    let store = Counted::with_stopwatch(slow, Arc::clone(&metrics), ticks.stopwatch());
    store
        .put_new(&key("a"), Bytes::from_static(b"x"), None)
        .await
        .expect("put");
    store.get_range(&key("a"), range(1)).await.expect("get");
    let page = scrape(&metrics);
    for (le, count) in [
        ("0.001", 0),
        ("0.0025", 0),
        ("0.005", 1),
        ("0.01", 1),
        ("+Inf", 1),
    ] {
        has(
            &page,
            &format!("kbf_cold_store_request_seconds_bucket{{op=\"get\",le=\"{le}\"}} {count}"),
        );
    }
    has(
        &page,
        "kbf_cold_store_request_seconds_sum{op=\"get\"} 0.003",
    );
    has(&page, "kbf_cold_store_request_seconds_count{op=\"get\"} 1");
    has(&page, "kbf_cold_store_request_seconds_count{op=\"put\"} 1");
    has(&page, "kbf_cold_store_request_seconds_count{op=\"head\"} 0");
    has(&page, "kbf_cold_store_request_seconds_count{op=\"list\"} 0");
    assert_eq!(metrics.seconds(Op::Get).count(), 1);
    assert_eq!(metrics.seconds(Op::Get).sum(), 3_000_000);
}

/// Catches: a family that is missing before the first request, a HELP or TYPE line that
/// drifts, a series that carries a key, bucket or host, and the cache's scrape leaving the
/// families out: the exact lines, zero, of the cold store's and the packs' families.
#[tokio::test]
async fn metrics_page_has_cold_store_families_and_help_lines() {
    assert_eq!(scrape(&StoreMetrics::cold()), COLD_ZERO);
    assert_eq!(scrape(&StoreMetrics::packs()), PACKS_ZERO);
    let cache = Cache::memory();
    let out = cache.render_metrics();
    assert!(
        out.contains(COLD_ZERO),
        "the cache's scrape has no cold store families"
    );
    for line in COLD_ZERO.lines().filter(|line| !line.starts_with('#')) {
        let labels = line.split_once('{').map_or("", |(_, rest)| rest);
        assert!(!labels.contains("key="), "{line}");
        assert!(
            !labels.contains("bucket=\"") || labels.contains("_bucket"),
            "{line}"
        );
        assert!(!labels.contains("host"), "{line}");
    }
}

/// Catches: a cache that renders a handle other than the one it was given (the families stay
/// at zero while the store is counted), so the store's requests never reach the scrape.
#[tokio::test]
async fn the_cache_renders_the_handle_it_is_given() {
    let metrics = Arc::new(StoreMetrics::cold());
    let store = Counted::new(memory(), Arc::clone(&metrics));
    store
        .put_new(&key("a"), Bytes::from_static(b"xyz"), None)
        .await
        .expect("put");
    let cache = Cache::memory().with_cold_store_metrics(Arc::clone(&metrics));
    let out = cache.render_metrics();
    has(&out, "kbf_cold_store_requests_total{op=\"put\"} 1");
    has(&out, "kbf_cold_store_bytes_total{dir=\"write\"} 3");
}

/// Catches: packs that render no store families, or a handle other than the one they were
/// given: zero lines before any request, and the store's counts once it has served one.
#[tokio::test]
async fn packs_render_the_pack_store_families() {
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let metrics = Arc::new(StoreMetrics::packs());
    let store = Arc::new(Counted::new(
        MemoryStore::new(create_only),
        Arc::clone(&metrics),
    ));
    let clock = Arc::new(kbf_state::ManualClock::default());
    let index: Arc<dyn PackIndex> = Arc::new(kbf_pack_index::memory::MemoryPackIndex::new(clock));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let packs = Packs::new(Arc::clone(&store), index, keys, PackSettings::default())
        .expect("packs")
        .with_store_metrics(Arc::clone(&metrics));
    let mut out = String::new();
    packs.render_metrics(&mut out);
    assert!(out.contains(PACKS_ZERO), "{out}");
    store
        .put_new(&key("p"), Bytes::from_static(b"12"), None)
        .await
        .expect("put");
    let mut out = String::new();
    Packs::render_metrics_of(Some(&packs), &mut out);
    has(&out, "kbf_pack_store_requests_total{op=\"put\"} 1");
}
