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
    let staged = std::fs::read_dir(config.path.join(".stage"))
        .unwrap()
        .count();
    assert_eq!(staged, 0);
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
    let staged = std::fs::read_dir(config.path.join(".stage"))
        .unwrap()
        .count();
    assert_eq!(staged, 0);
}
