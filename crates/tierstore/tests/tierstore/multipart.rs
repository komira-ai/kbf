//! Multipart uploads: the calls held to the contract of the trait on the in-memory and the
//! directory backend (the S3 backend is held to it too, in `s3.rs`, with the same functions),
//! the proxy over them (the cold tier takes the parts and makes the object), the fault injector
//! that counts and fails the calls, the cache that passes them on, and a backend that does none
//! of them.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};
use tierstore::{
    Backend, ByteRange, CachedStore, DirBackend, Error, Listed, MemoryBackend, Meta, Op, PartBody,
    Proxy, ProxyConfig, TestClock,
};

use crate::support::{Rig, assert_absent, body, journal_keys, keys, read, text};

/// The kinds of call of a multipart upload.
const MULTIPART_OPS: [Op; 4] = [
    Op::CreateUpload,
    Op::UploadPart,
    Op::CompleteUpload,
    Op::AbortUpload,
];

/// A body of these pieces.
fn pieces(texts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = texts.iter().map(|piece| Ok(body(piece))).collect();
    stream::iter(items).boxed()
}

/// The content tag of some bytes: the hex of their SHA-256.
fn tag_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The content tag a backend gives a part of some bytes.
pub type PartTag = fn(&[u8]) -> String;

/// Stores the part `number` of the upload `id` of `key` from these pieces, whose length is told
/// to the backend.
async fn part(
    backend: &dyn Backend,
    key: &str,
    id: &str,
    number: u32,
    texts: &[&'static str],
) -> Result<String, Error> {
    let body = pieces(texts);
    let len: u64 = texts.iter().map(|text| text.len() as u64).sum();
    let sized = backend.upload_part_with_len(key, id, number, Some(len), body);
    sized.await
}

/// Stores the part `number` of the upload `id` of `key` from these pieces, of a length that the
/// backend is not told.
async fn blind_part(
    backend: &dyn Backend,
    key: &str,
    id: &str,
    number: u32,
    texts: &[&'static str],
) -> Result<String, Error> {
    let body = pieces(texts);
    backend.upload_part(key, id, number, body).await
}

/// Completes the upload `id` of `key` with these parts.
async fn complete(
    backend: &dyn Backend,
    key: &str,
    id: &str,
    parts: &[(u32, String)],
) -> Result<String, Error> {
    backend.complete_upload(key, id, parts).await
}

/// Holds the multipart calls of `backend` to what the trait says: parts are stored from pieces,
/// with their length told or not, and replaced by number, the tag of a part is `part_tag` of its
/// bytes, parts are not objects, a completion that names parts wrongly or an upload that is not
/// held is refused, a completion creates the object and ends the upload and never replaces an
/// object, the tag of the object a completion makes is the one a head gives it (whether it is
/// also the tag of a put of the same bytes is for the backend's own tests: S3 tags an object
/// made of parts by its parts), a part whose body fails is not stored, and an abort ends an
/// upload.
pub async fn contract(backend: &dyn Backend, part_tag: PartTag) {
    let first = backend.create_upload("big").await.unwrap();
    let second = backend.create_upload("big").await.unwrap();
    assert_ne!(first, second);
    let two = blind_part(backend, "big", &first, 2, &["wor", "ld"])
        .await
        .unwrap();
    assert_eq!(two, part_tag(b"world"));
    let old = part(backend, "big", &first, 1, &["hello "]).await.unwrap();
    let one = part(backend, "big", &first, 1, &["HELLO ", ""])
        .await
        .unwrap();
    assert_eq!(one, part_tag(b"HELLO "));
    assert!(keys(backend).await.is_empty());
    assert_absent(backend, "big").await;

    let wrong = [
        vec![(3, two.clone())],
        vec![(1, old)],
        vec![(1, "nope".to_owned())],
        vec![(1, two.clone())],
    ];
    for parts in wrong {
        let refused = complete(backend, "big", &first, &parts).await;
        assert!(matches!(refused, Err(Error::InvalidPart(_))), "{parts:?}");
    }
    let parts = [(1, one), (2, two)];
    let wrong_key = ("other", first.as_str());
    for (key, id) in [("big", "nope"), ("big", "../x"), wrong_key] {
        let gone = Error::NotFound(id.to_owned());
        let done = complete(backend, key, id, &parts).await;
        assert_eq!(done, Err(gone.clone()));
        let stored = part(backend, key, id, 1, &["x"]).await;
        assert_eq!(stored, Err(gone.clone()));
        let aborted = backend.abort_upload(key, id).await;
        assert_eq!(aborted, Err(gone));
    }

    let tag = complete(backend, "big", &first, &parts).await.unwrap();
    assert_eq!(backend.head("big").await.unwrap().tag, tag);
    assert_eq!(text(backend, "big").await, "HELLO world");
    assert_eq!(keys(backend).await, ["big"]);
    let ended = complete(backend, "big", &first, &parts).await;
    assert!(matches!(ended, Err(Error::NotFound(_))));
    let late = part(backend, "big", &first, 3, &["x"]).await;
    assert!(matches!(late, Err(Error::NotFound(_))));

    let kept = part(backend, "big", &second, 1, &["z"]).await.unwrap();
    let taken = complete(backend, "big", &second, &[(1, kept)]).await;
    assert_eq!(taken, Err(Error::AlreadyExists("big".to_owned())));
    assert_eq!(text(backend, "big").await, "HELLO world");
    backend.abort_upload("big", &second).await.unwrap();

    let upload = backend.create_upload("cut").await.unwrap();
    let cut = Error::Other("cut".to_owned());
    let items: Vec<Result<Bytes, Error>> = vec![Ok(body("ab")), Err(cut.clone())];
    let failing: PartBody<'static> = stream::iter(items.clone()).boxed();
    let sized = backend.upload_part_with_len("cut", &upload, 1, Some(2), failing);
    assert_eq!(sized.await, Err(cut.clone()));
    let failing: PartBody<'static> = stream::iter(items).boxed();
    let stored = backend.upload_part("cut", &upload, 1, failing).await;
    assert_eq!(stored, Err(cut));
    let unstored = [(1, part_tag(b"ab"))];
    let refused = complete(backend, "cut", &upload, &unstored).await;
    assert!(matches!(refused, Err(Error::InvalidPart(_))));
    let mismatch = backend.abort_upload("elsewhere", &upload).await;
    assert!(matches!(mismatch, Err(Error::NotFound(_))));
    backend.abort_upload("cut", &upload).await.unwrap();
    let twice = backend.abort_upload("cut", &upload).await;
    assert_eq!(twice, Err(Error::NotFound(upload.clone())));
    let after = part(backend, "cut", &upload, 1, &["x"]).await;
    assert!(matches!(after, Err(Error::NotFound(_))));
    assert_absent(backend, "cut").await;
    assert_eq!(keys(backend).await, ["big"]);
}

/// A part larger than the pieces the backend joins parts in, and one after it: the object is
/// their join byte for byte. Gives the bytes of the object, which is left in the backend under
/// the key `large`.
pub async fn large(backend: &dyn Backend) -> Vec<u8> {
    let upload = backend.create_upload("large").await.unwrap();
    let megabyte = Bytes::from(vec![7_u8; 1 << 20]);
    let mut items = vec![Ok(megabyte.clone()), Ok(megabyte)];
    items.push(Ok(body("tail")));
    let big: PartBody<'static> = stream::iter(items).boxed();
    let len = Some((2 << 20) + 4);
    let sized = backend.upload_part_with_len("large", &upload, 1, len, big);
    let first = sized.await.unwrap();
    let last = part(backend, "large", &upload, 2, &["end"]).await.unwrap();
    let parts = [(1, first), (2, last)];
    let tag = complete(backend, "large", &upload, &parts).await.unwrap();
    assert_eq!(backend.head("large").await.unwrap().tag, tag);
    let mut whole = vec![7_u8; 2 << 20];
    whole.extend_from_slice(b"tailend");
    let size = backend.head("large").await.unwrap().size;
    assert_eq!(size, whole.len() as u64);
    let across = Some(ByteRange::new((2 << 20) - 2, 9));
    let got = backend.get("large", across).await.unwrap();
    assert_eq!(got, &b"\x07\x07tailend"[..]);
    assert_eq!(backend.get("large", None).await.unwrap(), whole);
    whole
}

/// Asserts that the in-memory backend does the calls of a multipart upload as the trait says,
/// and that the tag of a part and of the object it makes are the hex of the SHA-256 of the bytes.
#[tokio::test]
async fn the_memory_backend_does_multipart_uploads() {
    let backend = MemoryBackend::new();
    contract(&backend, tag_of).await;
    let tag = backend.head("big").await.unwrap().tag;
    assert_eq!(tag, tag_of(b"HELLO world"));
    let whole = large(&backend).await;
    let tag = backend.head("large").await.unwrap().tag;
    assert_eq!(tag, tag_of(&whole));
}

/// Asserts that the directory backend does the calls of a multipart upload as the trait says,
/// with parts of more than the pieces in which it joins them, gives the object made of parts the
/// tag of a put of the same bytes (the hex of their SHA-256), and keeps nothing of an upload
/// that is completed or aborted but the object.
#[tokio::test]
async fn the_directory_backend_does_multipart_uploads() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "multipart-dir");
    let backend = DirBackend::open(dir.path()).unwrap();
    contract(&backend, tag_of).await;
    let tag = backend.head("big").await.unwrap().tag;
    assert_eq!(tag, tag_of(b"HELLO world"));
    let whole = large(&backend).await;
    let tag = backend.head("large").await.unwrap().tag;
    assert_eq!(tag, tag_of(&whole));
    let uploads = std::fs::read_dir(dir.path().join(".uploads")).unwrap();
    assert_eq!(uploads.count(), 0);
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        names.push(entry.unwrap().file_name().to_string_lossy().into_owned());
    }
    names.sort();
    assert_eq!(names, [".uploads", "big", "large"]);
}

/// Asserts that a part whose upload is aborted while the part arrives is refused by the in-memory
/// backend as `NotFound`, and is not stored.
#[tokio::test]
async fn a_part_whose_upload_is_aborted_while_it_arrives_is_not_found() {
    let backend = Arc::new(MemoryBackend::new());
    let id = backend.create_upload("k").await.unwrap();
    let (aborting, upload) = (Arc::clone(&backend), id.clone());
    let arriving = stream::once(async move {
        aborting.abort_upload("k", &upload).await.unwrap();
        Ok(body("x"))
    });
    let arriving: PartBody<'static> = arriving.boxed();
    let stored = backend.upload_part("k", &id, 1, arriving).await;
    assert_eq!(stored, Err(Error::NotFound(id.clone())));
    let late = part(backend.as_ref(), "k", &id, 1, &["x"]).await;
    assert_eq!(late, Err(Error::NotFound(id)));
}

/// Asserts that an upload of the directory backend outlives the backend: a new backend over the
/// directory takes the next part and completes the upload, and a third one reads the object.
#[tokio::test]
async fn an_upload_of_the_directory_backend_survives_a_restart() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "multipart-restart");
    let before = DirBackend::open(dir.path()).unwrap();
    let id = before.create_upload("big").await.unwrap();
    let one = part(&before, "big", &id, 1, &["he"]).await.unwrap();
    drop(before);
    let after = DirBackend::open(dir.path()).unwrap();
    let two = part(&after, "big", &id, 2, &["llo"]).await.unwrap();
    let parts = [(1, one), (2, two)];
    let tag = complete(&after, "big", &id, &parts).await.unwrap();
    assert_eq!(tag, tag_of(b"hello"));
    let third = DirBackend::open(dir.path()).unwrap();
    assert_eq!(text(&third, "big").await, "hello");
}

/// Asserts that a multipart upload through the proxy goes to the cold tier only: the parts and
/// the completion are calls of the cold tier and none of the hot tier, the object is made in the
/// cold tier and nothing is journaled, a head, a listing and a read find it, the read copies it
/// into the hot tier, and a delete removes it.
#[tokio::test]
async fn a_multipart_upload_through_the_proxy_goes_to_the_cold_tier_only() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let id = proxy.create_upload("big").await.unwrap();
    let one = part(&proxy, "big", &id, 1, &["hello "]).await.unwrap();
    let two = part(&proxy, "big", &id, 2, &["world"]).await.unwrap();
    assert_eq!(rig.cold.calls(Op::UploadPart), 2);
    assert_eq!(rig.hot.calls(Op::UploadPart), 0);
    let journal = journal_keys(rig.hot_mem.as_ref(), "p1").await;
    let parts = [(1, one), (2, two)];
    let tag = complete(&proxy, "big", &id, &parts).await.unwrap();
    assert_eq!(journal_keys(rig.hot_mem.as_ref(), "p1").await, journal);
    assert_eq!(text(rig.cold_mem.as_ref(), "big").await, "hello world");
    assert_absent(rig.hot_mem.as_ref(), "big").await;
    assert_eq!(proxy.head("big").await.unwrap().tag, tag);
    assert_eq!(keys(&proxy).await, ["big"]);
    assert_eq!(read(&proxy, "big").await, "hello world");
    assert_eq!(text(rig.hot_mem.as_ref(), "big").await, "hello world");
    proxy.delete("big").await.unwrap();
    assert_absent(&proxy, "big").await;
    assert_absent(rig.cold_mem.as_ref(), "big").await;
}

/// Asserts that a completion through the proxy is create-only across both tiers: a key that the
/// hot tier holds, and one that only the cold tier holds, are refused with `AlreadyExists` and
/// the objects are as they were, the upload is left open and completes once the key is free, and
/// a hot tier that does not answer fails the completion before the cold tier is asked.
#[tokio::test]
async fn a_completion_through_the_proxy_never_replaces_an_object_of_either_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    proxy.put("hot", body("in hot")).await.unwrap();
    rig.cold_mem.put("cold", body("in cold")).await.unwrap();
    for key in ["hot", "cold"] {
        let id = proxy.create_upload(key).await.unwrap();
        let tag = part(&proxy, key, &id, 1, &["new"]).await.unwrap();
        let refused = complete(&proxy, key, &id, &[(1, tag)]).await;
        assert_eq!(refused, Err(Error::AlreadyExists(key.to_owned())));
        proxy.abort_upload(key, &id).await.unwrap();
    }
    assert_eq!(text(&proxy, "hot").await, "in hot");
    assert_eq!(text(&proxy, "cold").await, "in cold");

    let id = proxy.create_upload("cold").await.unwrap();
    let tag = part(&proxy, "cold", &id, 1, &["new"]).await.unwrap();
    let parts = [(1, tag)];
    rig.hot.fail_next(Op::Head, 1);
    let calls = rig.cold.calls(Op::CompleteUpload);
    let done = complete(&proxy, "cold", &id, &parts).await;
    assert!(done.unwrap_err().is_unavailable());
    assert_eq!(rig.cold.calls(Op::CompleteUpload), calls);
    proxy.delete("cold").await.unwrap();
    complete(&proxy, "cold", &id, &parts).await.unwrap();
    assert_eq!(text(rig.cold_mem.as_ref(), "cold").await, "new");
}

/// Asserts that the keys the proxy keeps for itself cannot be named by any call of a multipart
/// upload: each is refused with `Reserved` and the cold tier is not asked.
#[tokio::test]
async fn the_multipart_calls_of_the_proxy_refuse_its_reserved_keys() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let parts = [(1, "t".to_owned())];
    for key in ["_journal/p1/00000000000000000001", "_tiering/lease"] {
        let reserved = Error::Reserved(key.to_owned());
        assert_eq!(proxy.create_upload(key).await, Err(reserved.clone()));
        let stored = part(&proxy, key, "u", 1, &["x"]).await;
        assert_eq!(stored, Err(reserved.clone()));
        let done = complete(&proxy, key, "u", &parts).await;
        assert_eq!(done, Err(reserved.clone()));
        assert_eq!(proxy.abort_upload(key, "u").await, Err(reserved));
    }
    for op in MULTIPART_OPS {
        assert_eq!(rig.cold.calls(op), 0, "{op:?}");
    }
}

/// Asserts that the fault injector counts each multipart call and fails the next call of a kind
/// as it does the others, with `Unavailable`, and that the call after it goes through to the
/// backend it wraps.
#[tokio::test]
async fn every_multipart_call_can_be_counted_and_failed() {
    let rig = Rig::new();
    let cold = rig.cold.clone();
    for op in MULTIPART_OPS {
        cold.fail_next(op, 1);
    }
    let down = cold.create_upload("k").await.unwrap_err();
    assert!(down.is_unavailable());
    let id = cold.create_upload("k").await.unwrap();
    let stored = part(cold.as_ref(), "k", &id, 1, &["x"]).await;
    assert!(stored.unwrap_err().is_unavailable());
    let tag = part(cold.as_ref(), "k", &id, 1, &["x"]).await.unwrap();
    let parts = [(1, tag)];
    let failed = cold.complete_upload("k", &id, &parts).await;
    assert!(failed.unwrap_err().is_unavailable());
    let aborted = cold.abort_upload("k", &id).await;
    assert!(aborted.unwrap_err().is_unavailable());
    cold.complete_upload("k", &id, &parts).await.unwrap();
    assert_eq!(text(rig.cold_mem.as_ref(), "k").await, "x");
    assert_eq!(MULTIPART_OPS.map(|op| cold.calls(op)), [2, 2, 2, 1]);
    let late = cold.abort_upload("k", &id).await;
    assert!(matches!(late, Err(Error::NotFound(_))));
}

/// Asserts that the read cache passes the multipart calls on to the backend it is in front of,
/// and that the object they make is read and cached like any other.
#[tokio::test]
async fn the_cache_passes_multipart_calls_on() {
    let cache = CachedStore::new(Arc::new(MemoryBackend::new()), 1 << 20);
    let id = cache.create_upload("k").await.unwrap();
    let one = part(&cache, "k", &id, 1, &["ab", "cd"]).await.unwrap();
    let aborted = cache.create_upload("gone").await.unwrap();
    cache.abort_upload("gone", &aborted).await.unwrap();
    let tag = complete(&cache, "k", &id, &[(1, one)]).await.unwrap();
    assert_eq!(tag, tag_of(b"abcd"));
    assert_eq!(read(&cache, "k").await, "abcd");
    assert_eq!(cache.cached_bytes(), 4);
}

/// Asserts that a part passes through the proxy, the fault injector and the cache whether its
/// length is told or not: the cold tier of the proxy is told the length, or none, in the order
/// of the calls, a part of either kind is stored, and the proxy refuses a reserved key whichever
/// call is made, without asking the cold tier.
#[tokio::test]
async fn a_part_passes_through_the_wrappers_with_its_length_or_without() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let id = proxy.create_upload("k").await.unwrap();
    let blind = blind_part(&proxy, "k", &id, 1, &["ab"]).await.unwrap();
    let sized = part(&proxy, "k", &id, 2, &["cd"]).await.unwrap();
    assert_eq!(rig.cold.part_lengths(), [None, Some(2)]);
    let parts = [(1, blind), (2, sized)];
    complete(&proxy, "k", &id, &parts).await.unwrap();
    assert_eq!(text(rig.cold_mem.as_ref(), "k").await, "abcd");
    let key = "_tiering/lease";
    let reserved = blind_part(&proxy, key, "u", 1, &["x"]).await;
    assert_eq!(reserved, Err(Error::Reserved(key.to_owned())));
    let reserved = part(&proxy, key, "u", 1, &["x"]).await;
    assert_eq!(reserved, Err(Error::Reserved(key.to_owned())));
    assert_eq!(rig.cold.part_lengths(), [None, Some(2)]);

    let id = rig.cold.create_upload("direct").await.unwrap();
    let stored = blind_part(rig.cold.as_ref(), "direct", &id, 1, &["x"]);
    stored.await.unwrap();
    assert_eq!(rig.cold.part_lengths(), [None, Some(2), None]);

    let cache = CachedStore::new(Arc::new(MemoryBackend::new()), 1 << 20);
    let id = cache.create_upload("k").await.unwrap();
    blind_part(&cache, "k", &id, 1, &["x"]).await.unwrap();
    part(&cache, "k", &id, 2, &["y"]).await.unwrap();
    let parts = [(1, tag_of(b"x")), (2, tag_of(b"y"))];
    complete(&cache, "k", &id, &parts).await.unwrap();
    assert_eq!(text(&cache, "k").await, "xy");
}

/// A backend that does what the trait requires and no more: it holds nothing.
struct Plain;

#[async_trait]
impl Backend for Plain {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Full(key.to_owned()))
    }

    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}

/// Asserts that the error is `Unsupported` and names the call.
fn assert_unsupported(error: &Error, call: &str) {
    assert!(matches!(error, Error::Unsupported(_)), "{error}");
    assert!(error.to_string().contains(call), "{error}");
}

/// Asserts that a backend that does not override the multipart calls answers each of them
/// `Unsupported`, naming the call (the upload of a part with its length told is the upload of a
/// part), and so does a proxy whose cold tier is one.
#[tokio::test]
async fn a_backend_without_multipart_calls_answers_unsupported() {
    let plain = Plain;
    let created = plain.create_upload("k").await;
    let stored = part(&plain, "k", "u", 1, &["x"]).await;
    let blind = blind_part(&plain, "k", "u", 1, &["x"]).await;
    let completed = complete(&plain, "k", "u", &[]).await;
    let aborted = plain.abort_upload("k", "u").await;
    assert_unsupported(&created.unwrap_err(), "create_upload");
    assert_unsupported(&stored.unwrap_err(), "upload_part");
    assert_unsupported(&blind.unwrap_err(), "upload_part");
    assert_unsupported(&completed.unwrap_err(), "complete_upload");
    assert_unsupported(&aborted.unwrap_err(), "abort_upload");

    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let config = ProxyConfig::new("p1", 1 << 20, clock);
    let proxy = Proxy::open(hot, Arc::new(Plain), config).await.unwrap();
    let created = proxy.create_upload("k").await;
    let completed = complete(&proxy, "k", "u", &[]).await;
    assert_unsupported(&created.unwrap_err(), "create_upload");
    assert_unsupported(&completed.unwrap_err(), "complete_upload");
}
