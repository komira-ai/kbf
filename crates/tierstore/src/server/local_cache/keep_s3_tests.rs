//! The copy a put leaves in the cache when the upstream is an S3 bucket (the loopback S3 service
//! of the tests): the tag of an object that the S3 backend made from parts is not an MD5, so the
//! tag in the sidecar is only right if it is the one the upstream gives in a HEAD.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use kbf_objstore::s3::fake::FakeS3;
use kbf_testkit::dir::TestDir;

use super::layout::{Sidecar, meta_path, name_of};
use super::metrics::read;
use super::testing::{digest, pattern};
use super::{CacheConfig, LocalCache, SystemDisk};
use crate::s3::{Capabilities, Credentials, S3Config};
use crate::{Backend, FaultyBackend, Op, PartBody, S3Backend, TestClock};

/// A body of `bytes` in pieces of a mebibyte.
fn pieces(bytes: &[u8]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, crate::Error>> = bytes
        .chunks(1 << 20)
        .map(|piece| Ok(Bytes::copy_from_slice(piece)))
        .collect();
    stream::iter(items).boxed()
}

/// The turns of the scheduler a test gives an event before it fails.
const SPINS: u32 = 1_000_000;

/// Waits, by giving up the thread, until the stage directory is empty: the writer of a copy that
/// was dropped is aborted and removes its file when the runtime next runs it.
async fn drained(config: &CacheConfig) {
    let stage = config.path.join(".stage");
    for _ in 0..SPINS {
        if std::fs::read_dir(&stage).unwrap().count() == 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("after {SPINS} turns of the scheduler, the stage directory was not empty");
}

/// A cache of 64 MiB that keeps `cas/` and writes through, over a bucket of `s3`.
fn node(s3: &FakeS3, name: &str) -> (LocalCache, CacheConfig, TestDir) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let policies = vec![("cas/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
    config.write_through = true;
    let clock = Arc::new(TestClock::new());
    let cache = LocalCache::open(config.clone(), s3_backend(s3), Arc::new(SystemDisk), clock);
    (cache.unwrap(), config, dir)
}

/// A backend over the bucket of the loopback S3 service.
fn s3_backend(s3: &FakeS3) -> Arc<S3Backend> {
    let store = S3Config {
        endpoint: s3.endpoint().to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        min_body_rate: None,
    };
    Arc::new(S3Backend::new(store).unwrap())
}

/// Asserts that a put of a body over 16 MiB (which the S3 backend sends as a multipart upload)
/// and a put of a small one (one PUT) are each a local hit afterwards, with no call of the
/// upstream, and that the tag in each sidecar is the tag the upstream gives in a HEAD: for the
/// large object it is the tag of an object made of parts (it has a dash), which no hash of the
/// bytes could give. It fails on a keep that works the tag out from the bytes.
#[tokio::test]
async fn a_put_over_s3_is_kept_with_the_upstreams_tag_also_when_sent_as_parts() {
    let s3 = FakeS3::start().await;
    let backend = s3_backend(&s3);
    let upstream = Arc::new(FaultyBackend::new(backend.clone()));
    let dir = TestDir::under(std::env::temp_dir(), "tscache-keep-s3");
    let policies = vec![("cas/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
    config.write_through = true;
    let clock = Arc::new(TestClock::new());
    let cache = LocalCache::open(
        config.clone(),
        upstream.clone(),
        Arc::new(SystemDisk),
        clock,
    );
    let cache = cache.unwrap();

    let large = pattern(17 << 20);
    let small = pattern(3 << 20);
    for (key, bytes) in [("cas/b/parts", &large), ("cas/b/single", &small)] {
        let made = cache.put_stream(key, Some(bytes.len() as u64), pieces(bytes));
        assert_eq!(made.await, Ok(()), "{key}");
    }
    let stats = s3.stats();
    assert_eq!((stats.completed, stats.single_puts), (1, 1));
    assert!(stats.parts >= 2, "{stats:?}");

    for (key, bytes) in [("cas/b/parts", &large), ("cas/b/single", &small)] {
        let told = backend.head(key).await.unwrap();
        let path = meta_path(&config.path, &name_of(key).hex);
        let kept: Sidecar = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(kept.tag, told.tag, "{key}");
        assert_eq!(kept.size, bytes.len() as u64, "{key}");
        assert_eq!(kept.sha256, digest(bytes), "{key}");
    }
    let tag = backend.head("cas/b/parts").await.unwrap().tag;
    assert!(tag.contains('-'), "{tag}");

    let (gets, heads) = (upstream.calls(Op::Get), upstream.calls(Op::Head));
    assert_eq!(cache.get("cas/b/parts", None).await.unwrap(), large);
    assert_eq!(cache.get("cas/b/single", None).await.unwrap(), small);
    assert_eq!(
        (upstream.calls(Op::Get), upstream.calls(Op::Head)),
        (gets, heads)
    );
    assert_eq!(read(&cache.counters().hits), 2);
    assert_eq!(cache.held(), ((17 + 3) << 20, 2));
    assert_eq!(cache.inner.reserved.load(Ordering::SeqCst), 0);
    drained(&config).await;
}

/// Asserts that a key named by its content whose body hashes to another name stores nothing at an
/// S3 upstream, for a body that goes as one PUT and for one that goes as a multipart upload: the
/// put is `BadDigest`, the bucket has no object, no upload is left open, none was completed, and
/// the cache has nothing. It fails if the S3 backend makes the object when the error comes with
/// the last chunk.
#[tokio::test]
async fn a_name_mismatch_stores_nothing_over_s3() {
    for (label, size) in [("single", 1usize << 20), ("parts", 17 << 20)] {
        let s3 = FakeS3::start().await;
        let (cache, config, _dir) = node(&s3, &format!("tscache-keep-s3-bad-{label}"));
        let key = format!("cas/l/{}", digest(b"what the name says"));
        let bytes = pattern(size);
        let made = cache.put_stream(&key, Some(size as u64), pieces(&bytes));
        assert_eq!(
            made.await,
            Err(crate::Error::BadDigest(key.clone())),
            "{label}"
        );

        assert_eq!(s3.keys(), Vec::<String>::new(), "{label}");
        assert_eq!(s3.incomplete_uploads(), 0, "{label}");
        let stats = s3.stats();
        assert_eq!((stats.single_puts, stats.completed), (0, 0), "{label}");
        assert_eq!(cache.held(), (0, 0), "{label}");
        assert_eq!(cache.inner.reserved.load(Ordering::SeqCst), 0, "{label}");
        drained(&config).await;
    }
}

/// Asserts that a body over 16 MiB that is cut part of the way through leaves no object and no
/// open upload at the upstream, and nothing in the cache or its stage directory.
#[tokio::test]
async fn a_cut_multipart_sized_body_leaves_nothing_over_s3() {
    let s3 = FakeS3::start().await;
    let upstream = s3_backend(&s3);
    let dir = TestDir::under(std::env::temp_dir(), "tscache-keep-s3-cut");
    let policies = vec![("cas/".to_owned(), true)];
    let mut config = CacheConfig::new(dir.path().join("cache"), 64 << 20, policies);
    config.write_through = true;
    let clock = Arc::new(TestClock::new());
    let cache = LocalCache::open(config.clone(), upstream, Arc::new(SystemDisk), clock);
    let cache = cache.unwrap();

    let bytes = pattern(18 << 20);
    let mut items: Vec<Result<Bytes, crate::Error>> = bytes
        .chunks(1 << 20)
        .take(17)
        .map(|piece| Ok(Bytes::copy_from_slice(piece)))
        .collect();
    items.push(Err(crate::Error::Unavailable(
        "the client went away".to_owned(),
    )));
    let body: PartBody<'static> = stream::iter(items).boxed();
    let made = cache.put_stream("cas/b/cut", Some(18 << 20), body).await;
    assert!(made.is_err(), "{made:?}");

    assert_eq!(s3.keys(), Vec::<String>::new());
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(cache.held(), (0, 0));
    assert_eq!(cache.inner.reserved.load(Ordering::SeqCst), 0);
    drained(&config).await;
}
