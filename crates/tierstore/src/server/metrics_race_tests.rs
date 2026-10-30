//! The counters of the proxy's reads where the order of events matters: two reads that meet, a
//! read that is dropped, a hot tier that said it is full. The tiers are in memory and the order
//! is set by a barrier or a call that never returns, never by time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::FutureExt as _;
use tokio::sync::Barrier;

use crate::{
    Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op, Proxy, ProxyConfig,
    TestClock,
};

use super::metrics_tests::{
    BUDGET, COLD_BYTES, COLD_HIT_GET, COLD_SERVED, PROMOTED_BYTES, PROMOTIONS, sample, skipped,
};
use super::testing::NOW;

/// A memory tier whose gets wait for each other at a barrier, and whose puts can be made to
/// never return.
struct Held {
    inner: Arc<MemoryBackend>,
    gets: Option<Arc<Barrier>>,
    stall_puts: AtomicBool,
}

impl Held {
    fn new(inner: &Arc<MemoryBackend>, gets: Option<Arc<Barrier>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(inner),
            gets,
            stall_puts: AtomicBool::new(false),
        })
    }
}

#[async_trait]
impl Backend for Held {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        if self.stall_puts.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        if let Some(gate) = &self.gets {
            let _ = gate.wait().await;
        }
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// A proxy over `hot` and `cold`, whose usage is read again after `refresh`.
async fn proxy(hot: Arc<dyn Backend>, cold: Arc<dyn Backend>, refresh: Duration) -> Proxy {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let mut config = ProxyConfig::new("p1", BUDGET, clock);
    config.linger = Duration::from_millis(1);
    config.usage_refresh = refresh;
    Proxy::open(hot, cold, config).await.expect("a proxy")
}

fn memory() -> Arc<MemoryBackend> {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    Arc::new(MemoryBackend::with_clock(clock))
}

/// Asserts that two gets of one cold object that both read it before either copies it each count
/// a cold hit and the whole object read, and that one of them promotes and the other is
/// `raced`: two reads, one promotion. The two meet at a barrier in the cold tier, so both have
/// read before either puts. It would catch a promotion counted per read, and a lost race that is
/// counted as a failure.
#[tokio::test]
async fn two_concurrent_cold_gets_read_twice_and_promote_once() {
    let (hot, cold) = (memory(), memory());
    cold.put("obj", Bytes::from_static(b"twelve bytes"))
        .await
        .unwrap();
    let gate = Arc::new(Barrier::new(2));
    let proxy = proxy(hot, Held::new(&cold, Some(gate)), Duration::ZERO).await;
    let (one, two) = tokio::join!(proxy.get("obj", None), proxy.get("obj", None));
    assert_eq!(one.unwrap(), "twelve bytes");
    assert_eq!(two.unwrap(), "twelve bytes");
    let text = proxy.metrics();
    assert_eq!(sample(&text, COLD_HIT_GET), 2);
    assert_eq!(sample(&text, COLD_BYTES), 24);
    assert_eq!(sample(&text, COLD_SERVED), 24);
    assert_eq!(sample(&text, PROMOTIONS), 1);
    assert_eq!(sample(&text, PROMOTED_BYTES), 12);
    assert_eq!(sample(&text, &skipped("raced")), 1);
}

/// Asserts that a get that is dropped while it copies the object into the hot tier has counted
/// its cold hit and the bytes it read, and has counted neither a promotion nor a skipped copy:
/// promotions plus skips can be less than cold hits. The hot tier's put never returns, and the
/// get is polled once. It would catch a counter moved before the put (a promotion that did not
/// happen).
#[tokio::test]
async fn a_get_dropped_during_the_copy_counts_the_hit_and_no_promotion() {
    let (hot, cold) = (memory(), memory());
    cold.put("obj", Bytes::from_static(b"abc")).await.unwrap();
    let held = Held::new(&hot, None);
    let proxy = proxy(held.clone(), cold, Duration::ZERO).await;
    held.stall_puts.store(true, Ordering::SeqCst);
    assert!(proxy.get("obj", None).now_or_never().is_none());
    let text = proxy.metrics();
    assert_eq!(sample(&text, COLD_HIT_GET), 1);
    assert_eq!(sample(&text, COLD_BYTES), 3);
    assert_eq!(sample(&text, COLD_SERVED), 0);
    assert_eq!(sample(&text, PROMOTIONS), 0);
    for reason in ["critical", "full", "raced", "failed"] {
        assert_eq!(sample(&text, &skipped(reason)), 0, "{reason}");
    }
}

/// Asserts that a copy refused because the hot tier answered "full" counts `full`, and that the
/// next cold read, while the hot tier is taken to be full, counts `critical` and sends no put to
/// the hot tier: `critical` is the skip under a latched full as well as under the usage. The
/// usage is read again only after an hour, so the latch holds for the whole test. It would catch
/// a latched skip counted as `full`, or a skip that still sends the put.
#[tokio::test]
async fn a_copy_skipped_under_a_latched_full_counts_critical() {
    let (hot_mem, cold) = (memory(), memory());
    cold.put("a", Bytes::from_static(b"aaa")).await.unwrap();
    cold.put("b", Bytes::from_static(b"bbb")).await.unwrap();
    let hot = Arc::new(FaultyBackend::new(hot_mem));
    let proxy = proxy(hot.clone(), cold, Duration::from_secs(3600)).await;
    let before = hot.calls(Op::Put);
    hot.fail_next_with(Op::Put, Error::Full("a".to_owned()));
    assert_eq!(proxy.get("a", None).await.unwrap(), "aaa");
    let text = proxy.metrics();
    assert_eq!(sample(&text, &skipped("full")), 1);
    assert_eq!(sample(&text, &skipped("critical")), 0);
    assert_eq!(hot.calls(Op::Put), before + 1);
    assert_eq!(proxy.get("b", None).await.unwrap(), "bbb");
    let text = proxy.metrics();
    assert_eq!(sample(&text, &skipped("full")), 1);
    assert_eq!(sample(&text, &skipped("critical")), 1);
    assert_eq!(hot.calls(Op::Put), before + 1);
    assert_eq!(sample(&text, PROMOTIONS), 0);
}
