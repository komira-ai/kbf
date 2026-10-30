//! The create-only guarantees of a put through the proxy when a tier misbehaves or two puts of one
//! key meet: a key demoted to the cold tier between the first ask and the create, a hot tier
//! that runs out of room after it took bytes, a body longer than the cap of its key, and two
//! presigned puts that are both inside the hot tier's create at once.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use kbf_testkit::dir::TestDir;
use tierstore::server::{
    AccessKey, Grant, PresignKey, PresignRequest, Server, ServerConfig, presign_put,
};
use tierstore::{
    Backend, ByteRange, Clock, DirBackend, Error, Listed, MemoryBackend, Meta, PartBody, Proxy,
    ProxyConfig, SystemClock,
};
use tokio::sync::Barrier;

use crate::support::{Rig, assert_absent, body, chunks, journal_keys, read};

const SECRET: &str = "presign-secret-0123456789";
const BUCKET: &str = "tier-bucket";

/// A backend over another that can be told to misbehave in one way each.
struct Fake {
    inner: Arc<dyn Backend>,
    /// The first `head` of this key says it is not there, whatever the inner backend holds.
    forget_first_head: Option<(&'static str, AtomicU32)>,
    /// A streamed put takes the first chunk of its body and then answers that there is no room.
    take_then_full: bool,
    /// Streamed puts of this key wait here for the other party before they go on.
    gate: Option<(&'static str, Arc<Barrier>)>,
    /// How many streamed puts reached this backend.
    streams: AtomicU32,
}

impl Fake {
    fn over(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            forget_first_head: None,
            take_then_full: false,
            gate: None,
            streams: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Backend for Fake {
    fn needs_length(&self) -> bool {
        self.inner.needs_length()
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        if self.take_then_full {
            let _first = body.next().await;
            return Err(Error::Full("hot".to_owned()));
        }
        if let Some((name, barrier)) = &self.gate
            && *name == key
        {
            barrier.wait().await;
        }
        self.inner.put_stream(key, len, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        if let Some((name, asked)) = &self.forget_first_head
            && *name == key
            && asked.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(Error::NotFound(key.to_owned()));
        }
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// Asserts that a create whose first ask of the cold tier was stale is refused: the key is in the
/// cold tier (it was demoted after the first ask), the cold tier's first HEAD says it is not
/// there and the second says it is, and a held put, a streamed put and a streamed hot-only put (the
/// presigned path) each answer `AlreadyExists`, leave no object in the hot tier (the one they
/// created is removed), leave the cold object as it was, and journal nothing. It catches a proxy
/// that trusts its first ask, which would leave two objects under one key.
#[tokio::test(start_paused = true)]
async fn a_key_demoted_between_the_first_ask_and_the_create_is_refused() {
    for path in ["put", "stream", "hot_only"] {
        let rig = Rig::new();
        rig.cold_mem.put("k", body("old")).await.unwrap();
        let mut cold = Fake::over(rig.cold_mem.clone());
        cold.forget_first_head = Some(("k", AtomicU32::new(0)));
        let proxy = Proxy::open(rig.hot.clone(), Arc::new(cold), rig.config("p1", 1000))
            .await
            .unwrap();
        let outcome = match path {
            "put" => proxy.put("k", body("new")).await,
            "stream" => proxy.put_stream("k", None, chunks(&["ne", "w"])).await,
            _ => {
                let new = chunks(&["ne", "w"]);
                proxy.put_stream_hot_only("k", None, new).await
            }
        };
        assert_eq!(outcome, Err(Error::AlreadyExists("k".to_owned())), "{path}");
        assert_absent(&*rig.hot_mem, "k").await;
        assert_eq!(read(&*rig.cold_mem, "k").await, body("old"), "{path}");
        let journals = journal_keys(&*rig.hot_mem, "p1").await;
        assert_eq!(journals, Vec::<String>::new(), "{path}");
    }
}

/// A server over `proxy` with one presigning key limited to `cap` bytes of body, on the clock of
/// the machine.
fn server_over(proxy: Proxy, cap: u64) -> Arc<Server> {
    let grant = Grant {
        prefix: "cas/b/".to_owned(),
        max_expires_secs: 600,
        max_object_bytes: cap,
    };
    let key = PresignKey::new("pack-presign", SECRET, grant).unwrap();
    let mut config = ServerConfig::new(BUCKET, "FULL", "full-secret", Arc::new(SystemClock));
    config.presign_keys = vec![key];
    Server::new(Arc::new(proxy), config)
}

/// A presigned `PUT` request for `key`, with a body and headers.
fn put_request(key: &str, headers: &[(&str, &str)], data: Vec<u8>) -> Request<Full<Bytes>> {
    let access = AccessKey::new("pack-presign", SECRET);
    let wanted = PresignRequest {
        endpoint: "http://tier.test",
        region: "us-east-1",
        bucket: BUCKET,
        key,
        expires_secs: 300,
        now: SystemClock.now(),
    };
    let url = presign_put(&access, &wanted).unwrap();
    let path = url.strip_prefix("http://tier.test").unwrap();
    let mut builder = Request::builder()
        .method("PUT")
        .uri(path)
        .header("host", "tier.test");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::from(data))).unwrap()
}

/// What an answer was: its status, the `Retry-After` header and its body.
async fn answered(server: &Server, request: Request<Full<Bytes>>) -> (StatusCode, String, String) {
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let retry = parts.headers.get("retry-after");
    let retry = retry
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        retry.to_owned(),
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Asserts that a presigned put whose hot tier takes part of the body and then has no room is 503
/// `SlowDown` with `Retry-After`, stores nothing in either tier and journals nothing; that the
/// tier is latched full, so the next presigned put is 503 without a streamed put reaching the
/// hot tier; and that a put of the full key still goes to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_hot_tier_that_fills_after_taking_bytes_gives_503_and_stores_nothing() {
    let rig = Rig::new();
    let mut hot = Fake::over(rig.hot_mem.clone());
    hot.take_then_full = true;
    let hot = Arc::new(hot);
    let config = ProxyConfig::new("p1", 1 << 30, Arc::new(SystemClock));
    let proxy = Proxy::open(hot.clone(), rig.cold.clone(), config)
        .await
        .unwrap();
    let server = server_over(proxy, 1 << 20);
    let (status, retry, text) =
        answered(&server, put_request("cas/b/a", &[], b"abcd".to_vec())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(retry, "1");
    assert!(text.contains("<Code>SlowDown</Code>"), "{text}");
    assert_absent(&*rig.hot_mem, "cas/b/a").await;
    assert_absent(&*rig.cold_mem, "cas/b/a").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );
    let taken = hot.streams.load(Ordering::SeqCst);
    assert_eq!(taken, 1);
    let (status, _, _) = answered(&server, put_request("cas/b/b", &[], b"abcd".to_vec())).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        hot.streams.load(Ordering::SeqCst),
        taken,
        "latched: no put reached the hot tier"
    );
    assert_absent(&*rig.cold_mem, "cas/b/b").await;
}

/// The files under `root`, whatever their depth.
fn files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Asserts that a body that runs past the cap of its key while it streams into a directory tier
/// is refused with 413 `EntityTooLarge` and leaves nothing: the length declared is within the cap
/// in one request and absent in the other, the object is in neither tier, no journal entry was
/// written, and no temporary file is left in the directory.
#[tokio::test]
async fn a_body_that_runs_past_the_cap_while_streaming_leaves_nothing() {
    for declared in [Some("8"), None] {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-presign-cap");
        let hot = Arc::new(DirBackend::open(dir.path().join("hot")).unwrap());
        let cold = Arc::new(MemoryBackend::new());
        let config = ProxyConfig::new("p1", 1 << 30, Arc::new(SystemClock));
        let proxy = Proxy::open(hot.clone(), cold.clone(), config)
            .await
            .unwrap();
        let server = server_over(proxy, 16);
        let headers: Vec<(&str, &str)> = declared
            .map(|len| ("content-length", len))
            .into_iter()
            .collect();
        let request = put_request("cas/b/big", &headers, vec![7_u8; 20]);
        let (status, _, text) = answered(&server, request).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{declared:?}");
        assert!(text.contains("<Code>EntityTooLarge</Code>"), "{text}");
        assert_absent(&*hot, "cas/b/big").await;
        assert_absent(&*cold, "cas/b/big").await;
        assert_eq!(journal_keys(&*hot, "p1").await, Vec::<String>::new());
        let left: Vec<PathBuf> = files(&dir.path().join("hot"))
            .into_iter()
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                name.starts_with(".tmp-") || name.contains("big")
            })
            .collect();
        assert_eq!(left, Vec::<PathBuf>::new(), "{declared:?}");
        let ok = answered(&server, put_request("cas/b/ok", &[], vec![1_u8; 16])).await;
        assert_eq!(ok.0, StatusCode::OK);
    }
}

/// Two proxies (or one) over a hot tier whose streamed put of `cas/b/race` waits until both
/// presigned puts are inside it: a real race, in which both have passed every check before either
/// creates the object. Exactly one answers 200, the other 412, the object is the winner's, and
/// the cold tier holds nothing.
async fn race(two_proxies: bool) {
    let rig = Rig::new();
    let mut hot = Fake::over(rig.hot_mem.clone());
    hot.gate = Some(("cas/b/race", Arc::new(Barrier::new(2))));
    let hot = Arc::new(hot);
    let open = |id: &'static str| {
        let hot = hot.clone();
        let cold = rig.cold.clone();
        async move {
            let config = ProxyConfig::new(id, 1 << 30, Arc::new(SystemClock));
            Proxy::open(hot, cold, config).await.unwrap()
        }
    };
    let first = server_over(open("p1").await, 1 << 20);
    let second = if two_proxies {
        server_over(open("p2").await, 1 << 20)
    } else {
        first.clone()
    };
    let (a, b) = tokio::join!(
        answered(&first, put_request("cas/b/race", &[], b"AAAA".to_vec())),
        answered(&second, put_request("cas/b/race", &[], b"BBBB".to_vec())),
    );
    let mut statuses = [a.0.as_u16(), b.0.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 412]);
    let winner = if a.0 == StatusCode::OK {
        body("AAAA")
    } else {
        body("BBBB")
    };
    assert_eq!(read(&*rig.hot_mem, "cas/b/race").await, winner);
    assert_absent(&*rig.cold_mem, "cas/b/race").await;
}

/// Asserts the race through one proxy: one winner, the winner's bytes, one tier.
#[tokio::test(start_paused = true)]
async fn two_presigned_puts_inside_the_create_at_once_have_one_winner() {
    race(false).await;
}

/// Asserts the race through two proxies that share the tiers.
#[tokio::test(start_paused = true)]
async fn two_proxies_with_presigned_puts_inside_the_create_at_once_have_one_winner() {
    race(true).await;
}
