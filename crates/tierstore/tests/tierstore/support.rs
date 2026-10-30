//! Fixtures shared by the tests.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore::{
    Backend, ByteRange, Error, FaultyBackend, JournalEntry, MemoryBackend, PartBody, Proxy,
    ProxyConfig, Reconciler, ReconcilerConfig, TestClock, decode_journal,
};

/// A hot and a cold in-memory backend, each behind a fault injector, with one clock that
/// stamps the objects of both and the entries, leases and ages the store reads. The memory
/// backends are kept so a test can read or change a tier without going through the store.
pub struct Rig {
    pub clock: Arc<TestClock>,
    pub hot_mem: Arc<MemoryBackend>,
    pub cold_mem: Arc<MemoryBackend>,
    pub hot: Arc<FaultyBackend>,
    pub cold: Arc<FaultyBackend>,
}

impl Rig {
    pub fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        let hot_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let cold_mem = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let hot = Arc::new(FaultyBackend::new(hot_mem.clone()));
        let cold = Arc::new(FaultyBackend::new(cold_mem.clone()));
        Self {
            clock,
            hot_mem,
            cold_mem,
            hot,
            cold,
        }
    }

    /// The settings of a proxy `id` with a hot tier of `budget` bytes: a linger of a
    /// millisecond, a back-pressure of 5 milliseconds, and the usage read at every put.
    pub fn config(&self, id: &str, budget: u64) -> ProxyConfig {
        let mut config = ProxyConfig::new(id, budget, self.clock.clone());
        config.linger = Duration::from_millis(1);
        config.back_pressure = Duration::from_millis(5);
        config.usage_refresh = Duration::ZERO;
        config
    }

    /// Starts a proxy with these settings over the rig's backends.
    pub async fn open(&self, config: ProxyConfig) -> Result<Proxy, Error> {
        Proxy::open(self.hot.clone(), self.cold.clone(), config).await
    }

    /// A proxy `id` with a hot tier of `budget` bytes.
    pub async fn proxy(&self, id: &str, budget: u64) -> Proxy {
        self.open(self.config(id, budget)).await.unwrap()
    }

    /// The settings of a reconciler `holder` for a hot tier of `budget` bytes: the water
    /// marks at 0.8 and 0.7, a lease of 30 seconds and a retention of an hour.
    pub fn reconciler_config(&self, holder: &str, budget: u64) -> ReconcilerConfig {
        ReconcilerConfig::new(holder, budget, self.clock.clone())
    }

    /// A reconciler with these settings over the rig's backends.
    pub fn reconciler_with(&self, config: ReconcilerConfig) -> Reconciler {
        Reconciler::new(self.hot.clone(), self.cold.clone(), config)
    }

    /// A reconciler `holder` for a hot tier of `budget` bytes.
    pub fn reconciler(&self, holder: &str, budget: u64) -> Reconciler {
        self.reconciler_with(self.reconciler_config(holder, budget))
    }
}

/// The key of journal object `seq` of proxy `id`.
pub fn journal_key(id: &str, seq: u64) -> String {
    format!("_journal/{id}/{seq:020}")
}

/// The body of an object with this text.
pub fn body(text: &str) -> Bytes {
    Bytes::copy_from_slice(text.as_bytes())
}

/// The whole object under `key`.
pub async fn read(backend: &dyn Backend, key: &str) -> Bytes {
    backend.get(key, None).await.unwrap()
}

/// The text of the object under `key`.
pub async fn text(backend: &dyn Backend, key: &str) -> String {
    String::from_utf8(read(backend, key).await.to_vec()).unwrap()
}

/// The keys a backend lists under a prefix, in order.
pub async fn keys_under(backend: &dyn Backend, prefix: &str) -> Vec<String> {
    let listed = backend.list(prefix).await.unwrap();
    listed.into_iter().map(|object| object.key).collect()
}

/// The keys a backend lists, in order.
pub async fn keys(backend: &dyn Backend) -> Vec<String> {
    keys_under(backend, "").await
}

/// The keys of the page of the objects of `backend` after `start`.
pub async fn page(backend: &dyn Backend, start: Option<&str>, max_keys: usize) -> Vec<String> {
    let listed = backend.list_page("", start, max_keys).await.unwrap();
    listed.into_iter().map(|object| object.key).collect()
}

/// Asserts that the backend holds no object under `key`.
pub async fn assert_absent(backend: &dyn Backend, key: &str) {
    let error = backend.head(key).await.unwrap_err();
    assert_eq!(error, Error::NotFound(key.to_owned()));
}

/// The keys of the journal objects of proxy `id` in the backend.
pub async fn journal_keys(backend: &dyn Backend, id: &str) -> Vec<String> {
    keys_under(backend, &format!("_journal/{id}/")).await
}

/// The entries of journal object `seq` of proxy `id`.
pub async fn journal(backend: &dyn Backend, id: &str, seq: u64) -> Vec<JournalEntry> {
    let object = read(backend, &journal_key(id, seq)).await;
    decode_journal(&object).unwrap()
}

/// The keys of the entries of journal object `seq` of proxy `id`.
pub async fn journal_entry_keys(backend: &dyn Backend, id: &str, seq: u64) -> Vec<String> {
    let entries = journal(backend, id, seq).await;
    entries.into_iter().map(|entry| entry.key).collect()
}

/// Replaces the usage object the reconciler would publish, in the hot tier directly.
pub async fn set_usage(rig: &Rig, hot_bytes: u64, time: u64) {
    let text = format!(r#"{{"hot_bytes":{hot_bytes},"copy_lag":0,"time":{time}}}"#);
    rig.hot_mem.delete("_tiering/usage").await.unwrap();
    rig.hot_mem
        .put("_tiering/usage", body(&text))
        .await
        .unwrap();
}

/// Reads ranges of the ten-byte object `0123456789` under `key` and checks every one.
pub async fn check_ranges(backend: &dyn Backend, key: &str) {
    let cases = [
        (0, 1, "0"),
        (9, 1, "9"),
        (4, 3, "456"),
        (8, 100, "89"),
        (4, u64::MAX, "456789"),
        (3, 0, ""),
        (10, 0, ""),
        (10, 5, ""),
    ];
    for (offset, len, want) in cases {
        let range = Some(ByteRange::new(offset, len));
        let got = backend.get(key, range).await.unwrap();
        assert_eq!(got, body(want), "range {offset}+{len}");
    }
    let whole = backend.get(key, None).await.unwrap();
    assert_eq!(whole, body("0123456789"));
    let past = backend.get(key, Some(ByteRange::new(11, 1))).await;
    assert!(matches!(past, Err(Error::Other(_))));
}

/// A body of these chunks, for a streamed put.
pub fn chunks(texts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = texts.iter().map(|text| Ok(body(text))).collect();
    stream::iter(items).boxed()
}

/// A body of these chunks that then ends with `error`, as a body does that was cut or whose
/// checksum did not match.
pub fn broken(texts: &[&'static str], error: &Error) -> PartBody<'static> {
    let mut items: Vec<Result<Bytes, Error>> = texts.iter().map(|text| Ok(body(text))).collect();
    items.push(Err(error.clone()));
    stream::iter(items).boxed()
}
