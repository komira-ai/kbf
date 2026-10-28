//! `Packs` itself: how it is made, what it checks at start, and the rules of its own that no
//! call through the proxy can reach (a blob that lies about its digest, a bound, a cache).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{MAX_PACKED_BLOB_BYTES, PackSettings, Packs};
use kbf_pack::{FRONTEND_PACK_BYTES, PackError, PackKeys, ReadCache, WriterKind};
use kbf_pack_index::{MAX_BATCH, PackIndex};
use kbf_types::{Digest, DigestFunction};
use tonic::Code;

use crate::support::{Index, Rig, Spy, blob};

fn keys() -> PackKeys {
    PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys")
}

/// Packs over `store` and a fresh index.
fn packs_over(store: &Arc<Spy>, settings: PackSettings) -> Result<Packs, PackError> {
    let index: Arc<dyn PackIndex> = Arc::new(Index::over(Arc::clone(store)));
    Packs::new(Arc::clone(store), index, keys(), settings)
}

/// Asserts the defaults: packs of the frontend's size, a bound of 64 MiB, room for 1 GiB of
/// uploads in flight, an idle timeout of 30 seconds and no cache; and that the packs print
/// without the store.
#[tokio::test]
async fn the_defaults_are_the_frontends() {
    let settings = PackSettings::default();
    assert_eq!(settings.pack_bytes, FRONTEND_PACK_BYTES);
    assert_eq!(settings.max_blob_bytes, MAX_PACKED_BLOB_BYTES);
    assert_eq!(MAX_PACKED_BLOB_BYTES, 64 << 20);
    assert_eq!(settings.inflight_bytes, kbf_front::DEFAULT_INFLIGHT_BYTES);
    assert_eq!(kbf_front::DEFAULT_INFLIGHT_BYTES, 1 << 30);
    assert_eq!(settings.upload_idle_timeout, Duration::from_secs(30));
    assert_eq!(
        settings.upload_idle_timeout,
        kbf_front::DEFAULT_UPLOAD_IDLE_TIMEOUT
    );
    assert!(settings.cache.is_none());

    let rig = Rig::new().await;
    assert_eq!(rig.packs.max_blob_bytes(), MAX_PACKED_BLOB_BYTES);
    assert_eq!(rig.packs.inflight_bytes(), settings.inflight_bytes);
    let idle = rig.packs.upload_idle_timeout();
    assert_eq!(idle, settings.upload_idle_timeout);
    let printed = format!("{:?}", rig.packs);
    assert!(printed.contains("max_blob_bytes"), "{printed}");
}

/// Asserts that the idle timeout a frontend is configured with is the one its packs keep.
#[tokio::test]
async fn the_configured_idle_timeout_is_the_one_the_packs_keep() {
    let settings = PackSettings {
        upload_idle_timeout: Duration::from_secs(7),
        ..PackSettings::default()
    };
    let rig = Rig::with(settings).await;

    assert_eq!(rig.packs.upload_idle_timeout(), Duration::from_secs(7));
}

/// Asserts that packs are refused over a store that does not claim create-only writes, which
/// the write rule needs.
#[tokio::test]
async fn a_store_without_create_only_writes_is_refused() {
    let store = Arc::new(Spy::without_create_only());

    let refused = packs_over(&store, PackSettings::default());

    assert!(matches!(refused, Err(PackError::NotCreateOnly)));
}

/// Asserts the startup check: a store that refuses to replace an object passes, and one that
/// claims it and replaces fails with the reason: a store is not taken on its word.
#[tokio::test]
async fn the_create_only_check_tests_the_claim() {
    let honest = Arc::new(Spy::new());
    let packs = packs_over(&honest, PackSettings::default()).expect("packs");
    let checked = packs.check_create_only().await;
    checked.expect("refuses a second write");

    let liar = Arc::new(Spy::replacing());
    let packs = packs_over(&liar, PackSettings::default()).expect("claims create-only");
    let failed = packs.check_create_only().await;
    let said = failed.expect_err("took a second write");
    assert!(said.contains("second write"), "{said}");
}

/// Asserts that a blob whose bytes are not its digest is refused INVALID_ARGUMENT before
/// anything is written, and is not counted as a failure to write a pack: the client's mistake
/// is not the store's or the index's.
#[tokio::test]
async fn a_blob_that_lies_about_its_digest_is_refused() {
    let rig = Rig::new().await;
    let (digest, _) = blob("what the digest names");

    let refused = rig
        .packs
        .put(vec![(digest, Bytes::from_static(b"what the bytes are"))])
        .await;

    assert_eq!(refused.expect_err("refused").code(), Code::InvalidArgument);
    assert_eq!(rig.store.puts(), 0);
    assert_eq!(rig.packs.metrics().write_failures.get(), 0);
}

/// Asserts that the index is asked in batches it takes: a question about one more digest than
/// a batch holds is answered, all of them absent, in the order asked.
#[tokio::test]
async fn missing_asks_the_index_in_batches() {
    let rig = Rig::new().await;
    let asked: Vec<Digest> = (0..=MAX_BATCH as u64)
        .map(|i| Digest::new(DigestFunction::Sha256, [7; 32], i))
        .collect();

    let absent = rig.packs.missing(&asked).await;

    assert_eq!(absent.expect("answered"), asked);
}

/// Asserts that a blob over the bound is not looked up: the read answers `None` without asking
/// the index, which a failure armed for its next lookup shows by still being there for the
/// next read.
#[tokio::test]
async fn a_blob_over_the_bound_is_not_looked_up() {
    let settings = PackSettings {
        max_blob_bytes: 4,
        ..PackSettings::default()
    };
    let rig = Rig::with(settings).await;
    let (small, _) = blob("tiny");
    let (big, _) = blob("bigger than four");
    rig.index.fail_locate(1);

    let over = rig.packs.read(&big).await;
    assert!(over.expect("answered").is_none());

    let within = rig.packs.read(&small).await;
    let refused = within.expect_err("the index fails");
    assert_eq!(refused.code(), Code::Unavailable);
}

/// Asserts that blobs read from packs are kept in the cache: the second read of a blob is
/// served without a request to the store.
#[tokio::test]
async fn a_cache_serves_the_second_read() {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "front-pack-cache");
    let cache = ReadCache::open(dir.path(), 1 << 20).expect("a cache");
    let settings = PackSettings {
        cache: Some(Arc::new(cache)),
        ..PackSettings::default()
    };
    let rig = Rig::with(settings).await;
    let (digest, bytes) = blob("kept on the disk");
    let packed = rig.packs.put(vec![(digest, bytes.clone())]).await;
    packed.expect("packed");

    let first = rig.packs.read(&digest).await.expect("read");
    let second = rig.packs.read(&digest).await.expect("read");

    assert_eq!(first, Some(bytes.clone()));
    assert_eq!(second, Some(bytes));
    assert_eq!(rig.store.ranges().len(), 1);
}
