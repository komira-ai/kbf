//! `Backend::list_uploads`: the call held to the contract of the trait on the in-memory and the
//! directory backend (the S3 backend is held to it too, in `s3.rs`, with the same function), the
//! proxy over them (the cold tier's uploads, without the reserved keys), the fault injector that
//! counts and fails the call, the cache that passes it on, and a backend that does not do it.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_testkit::dir::TestDir;
use tierstore::{
    Backend, CachedStore, DirBackend, Error, MemoryBackend, Op, OpenUpload, TestClock,
};

use crate::support::Rig;

/// The key of each upload.
fn keys_of(uploads: &[OpenUpload]) -> Vec<&str> {
    uploads.iter().map(|upload| upload.key.as_str()).collect()
}

/// The id of each upload, as a set.
fn ids_of(uploads: &[OpenUpload]) -> BTreeSet<String> {
    uploads.iter().map(|upload| upload.id.clone()).collect()
}

/// Stores `text` as the part 1 of the upload `id` of `key`, and completes the upload with it.
pub async fn finish(backend: &dyn Backend, key: &str, id: &str, text: &'static str) {
    let items: Vec<Result<Bytes, Error>> = vec![Ok(Bytes::from_static(text.as_bytes()))];
    let body = stream::iter(items).boxed();
    let len = Some(text.len() as u64);
    let tag = backend.upload_part_with_len(key, id, 1, len, body).await;
    let parts = [(1, tag.unwrap())];
    backend.complete_upload(key, id, &parts).await.unwrap();
}

/// Asserts that a backend lists its open uploads as the trait says: all of them in the order of
/// key, those under a prefix, at most the number asked for, pages that follow the markers (a key
/// and an id, or a key alone, which skips every upload of the key) and end with a short page,
/// and none that was completed or aborted. It catches a listing that ignores the prefix, the
/// limit or a marker, repeats or drops the upload on the border of two pages, skips the second
/// upload of a key with a key marker that names an id, or keeps an upload after it ends.
pub async fn contract(backend: &dyn Backend) {
    let first = backend.create_upload("a/one").await.unwrap();
    let second = backend.create_upload("a/two").await.unwrap();
    let again = backend.create_upload("a/two").await.unwrap();
    let last = backend.create_upload("b/one").await.unwrap();

    let all = backend.list_uploads("", None, 100).await.unwrap();
    assert_eq!(keys_of(&all), ["a/one", "a/two", "a/two", "b/one"]);
    let made = [first.clone(), second, again, last.clone()];
    assert_eq!(ids_of(&all), BTreeSet::from(made));
    assert_eq!(all[0].id, first);

    let under = |prefix: &'static str| async move {
        backend.list_uploads(prefix, None, 100).await.unwrap()
    };
    assert_eq!(under("a/").await, all[..3]);
    assert_eq!(under("b/").await, all[3..]);
    assert_eq!(under("a/t").await, all[1..3]);
    assert!(under("c/").await.is_empty());
    assert!(under("a/three").await.is_empty());

    assert_eq!(backend.list_uploads("", None, 2).await.unwrap(), all[..2]);
    assert!(backend.list_uploads("", None, 0).await.unwrap().is_empty());

    let page = backend.list_uploads("", None, 3).await.unwrap();
    assert_eq!(page, all[..3]);
    let end = (page[2].key.as_str(), Some(page[2].id.as_str()));
    let rest = backend.list_uploads("", Some(end), 3).await.unwrap();
    assert_eq!(rest, all[3..]);
    let end = (rest[0].key.as_str(), Some(rest[0].id.as_str()));
    assert!(
        backend
            .list_uploads("", Some(end), 3)
            .await
            .unwrap()
            .is_empty()
    );

    let key_only = |key: &'static str| async move {
        backend
            .list_uploads("", Some((key, None)), 100)
            .await
            .unwrap()
    };
    assert_eq!(key_only("a/one").await, all[1..]);
    assert_eq!(key_only("a/two").await, all[3..]);
    assert!(key_only("b/one").await.is_empty());
    assert_eq!(key_only("a").await, all);

    let middle = (all[1].key.as_str(), Some(all[1].id.as_str()));
    let after_first = backend.list_uploads("", Some(middle), 100).await.unwrap();
    assert_eq!(after_first, all[2..]);
    let middle = (all[2].key.as_str(), Some(all[2].id.as_str()));
    let after_second = backend.list_uploads("", Some(middle), 100).await.unwrap();
    assert_eq!(after_second, all[3..]);
    let paged = backend.list_uploads("b/", Some(("a/two", None)), 100).await;
    assert_eq!(paged.unwrap(), all[3..]);

    finish(backend, "a/one", &first, "bytes").await;
    backend.abort_upload("b/one", &last).await.unwrap();
    let left = backend.list_uploads("", None, 100).await.unwrap();
    assert_eq!(keys_of(&left), ["a/two", "a/two"]);
    assert_eq!(left, all[1..3]);
    let ended = backend.list_uploads("a/o", None, 100).await.unwrap();
    assert!(ended.is_empty());
}

/// Asserts that the in-memory backend lists its uploads as the trait says, and stamps each with
/// the time of its clock when it was started.
#[tokio::test]
async fn the_memory_backend_lists_its_open_uploads() {
    let clock = Arc::new(TestClock::new());
    let backend = MemoryBackend::with_clock(Arc::clone(&clock));
    clock.set(1_000);
    let early = backend.create_upload("k/early").await.unwrap();
    clock.set(2_500);
    let late = backend.create_upload("k/late").await.unwrap();
    let listed = backend.list_uploads("k/", None, 10).await.unwrap();
    let want = [
        OpenUpload {
            key: "k/early".to_owned(),
            id: early,
            initiated: 1_000,
        },
        OpenUpload {
            key: "k/late".to_owned(),
            id: late,
            initiated: 2_500,
        },
    ];
    assert_eq!(listed, want);
    contract(&MemoryBackend::new()).await;
}

/// Asserts that the directory backend lists its uploads as the trait says, that each carries a
/// time, that a directory in the uploads directory with no file for the key (an upload that is
/// being made or being removed) is not an upload, and that an upload is listed again by a backend
/// opened over the same directory.
#[tokio::test]
async fn the_directory_backend_lists_its_open_uploads() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "list-uploads-dir");
    let backend = DirBackend::open(dir.path()).unwrap();
    contract(&backend).await;
    let id = backend.create_upload("kept/one").await.unwrap();
    let listed = backend.list_uploads("kept/", None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].initiated > 0, "{listed:?}");
    std::fs::create_dir(dir.path().join(".uploads").join("half-made")).unwrap();
    let with_stray = backend.list_uploads("kept/", None, 10).await.unwrap();
    assert_eq!(with_stray, listed);
    assert_eq!(backend.list_uploads("", None, 10).await.unwrap().len(), 3);
    let reopened = DirBackend::open(dir.path()).unwrap();
    let again = reopened.list_uploads("kept/", None, 10).await.unwrap();
    assert_eq!(again, listed);
    assert_eq!(again[0].id, id);
}

/// Asserts that the proxy lists the uploads of the cold tier and asks the hot tier nothing: the
/// uploads it started, and one another writer started in the cold tier, by prefix and by page.
#[tokio::test]
async fn the_proxy_lists_the_open_uploads_of_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let through = proxy.create_upload("jobs/a").await.unwrap();
    let direct = rig.cold_mem.create_upload("jobs/b").await.unwrap();
    let other = proxy.create_upload("other/c").await.unwrap();

    let all = proxy.list_uploads("", None, 10).await.unwrap();
    assert_eq!(keys_of(&all), ["jobs/a", "jobs/b", "other/c"]);
    let want = BTreeSet::from([through, direct, other]);
    assert_eq!(ids_of(&all), want);
    let jobs = proxy.list_uploads("jobs/", None, 10).await.unwrap();
    assert_eq!(jobs, all[..2]);
    let end = (all[0].key.as_str(), Some(all[0].id.as_str()));
    let rest = proxy.list_uploads("", Some(end), 10).await.unwrap();
    assert_eq!(rest, all[1..]);
    assert_eq!(rig.hot.calls(Op::ListUploads), 0);
    assert_eq!(rig.cold.calls(Op::ListUploads), 3);
    let by_cold = rig.cold_mem.list_uploads("", None, 10).await.unwrap();
    assert_eq!(by_cold, all);
}

/// Asserts that the proxy leaves out the uploads of the keys it keeps for itself, wherever the
/// listing starts and whatever prefix it asks for, and that it asks the cold tier for the page
/// again until the page is full or the cold tier has no more, so a short page is the last. It
/// catches a proxy that lists `_journal/` and `_tiering/` uploads, and one that cuts a page
/// short with the filter.
#[tokio::test]
async fn the_proxy_never_lists_an_upload_of_a_reserved_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    for key in ["_journal/p1/0", "_journal/p1/1", "_tiering/lease"] {
        rig.cold_mem.create_upload(key).await.unwrap();
    }
    for key in ["a/1", "a/2", "a/3"] {
        proxy.create_upload(key).await.unwrap();
    }
    let all = proxy.list_uploads("", None, 100).await.unwrap();
    assert_eq!(keys_of(&all), ["a/1", "a/2", "a/3"]);
    for prefix in ["_", "_journal/", "_tiering/lease"] {
        let listed = proxy.list_uploads(prefix, None, 100).await.unwrap();
        assert!(listed.is_empty(), "{prefix}: {listed:?}");
    }
    let before = rig.cold.calls(Op::ListUploads);
    let page = proxy.list_uploads("", None, 2).await.unwrap();
    assert_eq!(page, all[..2]);
    assert_eq!(rig.cold.calls(Op::ListUploads), before + 3);
    let end = (all[1].key.as_str(), Some(all[1].id.as_str()));
    let last = proxy.list_uploads("", Some(end), 2).await.unwrap();
    assert_eq!(last, all[2..]);
    let before = rig.cold.calls(Op::ListUploads);
    let none = proxy.list_uploads("", None, 0).await.unwrap();
    assert!(none.is_empty());
    assert_eq!(rig.cold.calls(Op::ListUploads), before);
}

/// Asserts that the fault injector counts the listing as a call of its own kind, fails it where
/// told to, and passes the answer and the markers on, and that the proxy gives the failure of
/// the cold tier as it is.
#[tokio::test]
async fn the_listing_of_uploads_can_be_counted_and_failed() {
    let rig = Rig::new();
    let cold = rig.cold.clone();
    let id = cold.create_upload("k").await.unwrap();
    cold.fail_next(Op::ListUploads, 1);
    let down = cold.list_uploads("", None, 10).await.unwrap_err();
    assert!(down.is_unavailable());
    let listed = cold.list_uploads("", None, 10).await.unwrap();
    assert_eq!(ids_of(&listed), BTreeSet::from([id.clone()]));
    let past = cold.list_uploads("", Some(("k", None)), 10).await.unwrap();
    assert!(past.is_empty());
    assert_eq!(cold.calls(Op::ListUploads), 3);

    let proxy = rig.proxy("p1", 1 << 20).await;
    cold.fail_next(Op::ListUploads, 1);
    let failed = proxy.list_uploads("", None, 10).await.unwrap_err();
    assert!(failed.is_unavailable());
    let refilled = proxy.list_uploads("", None, 10).await.unwrap();
    assert_eq!(ids_of(&refilled), BTreeSet::from([id]));
}

/// Asserts that the read cache passes the listing on to the backend it is in front of.
#[tokio::test]
async fn the_cache_passes_the_listing_of_uploads_on() {
    let inner = Arc::new(MemoryBackend::new());
    let cache = CachedStore::new(inner.clone(), 1 << 20);
    let id = inner.create_upload("k").await.unwrap();
    let listed = cache.list_uploads("", None, 10).await.unwrap();
    assert_eq!(ids_of(&listed), BTreeSet::from([id]));
    let past = cache.list_uploads("", Some(("k", None)), 10).await.unwrap();
    assert!(past.is_empty());
}
