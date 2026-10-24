//! The S3 client of kbf against the S3 front, over loopback: the calls kbf makes, and what it
//! gets back for them.

use std::fmt::Debug;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_objstore::s3::{MULTIPART_THRESHOLD, S3Store};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use tierstore::{Backend, Error, Op};

use crate::support::Harness;

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// The bytes `[offset, offset + len)`.
fn window(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).unwrap()
}

/// Puts `body` under `name`.
async fn put(store: &S3Store, name: &str, body: &'static [u8]) {
    let body = Bytes::from_static(body);
    store.put_new(&key(name), body, None).await.unwrap();
}

/// Reads `len` bytes of `name` from `offset`.
async fn read(store: &S3Store, name: &str, offset: u64, len: u64) -> Bytes {
    let bytes = window(offset, len);
    store.get_range(&key(name), bytes).await.unwrap()
}

/// Asserts that a call was answered with this status and S3 code.
fn assert_refused<T: Debug>(result: Result<T, ObjectStoreError>, status: u16, code: &str) {
    match result.unwrap_err() {
        ObjectStoreError::Service {
            status: got,
            code: found,
            ..
        } => {
            assert_eq!(got, status);
            assert_eq!(found, code);
        }
        other => panic!("not an S3 answer: {other}"),
    }
}

/// Asserts that an object put with the client is stored in the hot tier, and is read back whole
/// and by range with the exact `Content-Range` the client checks, a range past the end cut and
/// a range that starts at the end refused, that a second put of the key is `AlreadyExists`, and
/// that a delete succeeds twice and leaves the key `NotFound`.
#[tokio::test]
async fn an_object_is_put_read_by_range_and_deleted() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(CONDITIONAL);
    put(&store, "cas/pack-1", b"hello").await;
    assert_eq!(h.hot_mem.get("cas/pack-1", None).await.unwrap(), "hello");
    assert_eq!(read(&store, "cas/pack-1", 0, 5).await, "hello");
    assert_eq!(read(&store, "cas/pack-1", 1, 3).await, "ell");
    assert_eq!(read(&store, "cas/pack-1", 3, 100).await, "lo");
    let k = key("cas/pack-1");
    let past = store.get_range(&k, window(5, 1)).await.unwrap_err();
    assert!(matches!(past, ObjectStoreError::InvalidRange { .. }));
    let body = Bytes::from_static(b"other");
    let again = store.put_new(&k, body, None).await;
    assert!(matches!(again, Err(ObjectStoreError::AlreadyExists(_))));
    assert_eq!(read(&store, "cas/pack-1", 0, 5).await, "hello");
    store.delete(&k).await.unwrap();
    store.delete(&k).await.unwrap();
    let gone = store.get_range(&k, window(0, 5)).await.unwrap_err();
    assert!(matches!(gone, ObjectStoreError::NotFound(_)), "{gone}");
}

/// Asserts that a client that does not claim conditional puts also gets create-only puts: a
/// second put of a key is answered 412 `PreconditionFailed` and the first object is kept.
#[tokio::test]
async fn a_put_without_the_header_does_not_replace_either() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(Capabilities::default());
    put(&store, "cas/pack-2", b"first").await;
    let second = Bytes::from_static(b"second");
    let result = store.put_new(&key("cas/pack-2"), second, None).await;
    assert_refused(result, 412, "PreconditionFailed");
    assert_eq!(read(&store, "cas/pack-2", 0, 5).await, "first");
}

/// Asserts that the head the client sends before it deletes in a bucket with Object Lock is
/// answered: the delete of a key that is there removes it, and the delete of one that is not
/// is a success.
#[tokio::test]
async fn a_head_is_answered_for_a_client_that_locks_objects() {
    let h = Harness::start(|_| {}).await;
    let locking = Capabilities {
        conditional_put: true,
        object_lock: true,
    };
    let store = h.store(locking);
    put(&h.store(CONDITIONAL), "audit/entry", b"x").await;
    store.delete(&key("audit/entry")).await.unwrap();
    assert!(h.hot_mem.get("audit/entry", None).await.is_err());
    store.delete(&key("audit/entry")).await.unwrap();
}

/// Asserts that a listing in pages follows the client's tokens through every key under the prefix
/// once and in order, with sizes, that keys of the cold tier are in it too, that a listing after
/// a key starts after it, and that the keys the proxy keeps for itself are not listed.
#[tokio::test]
async fn a_listing_pages_through_both_tiers() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(CONDITIONAL);
    for n in 0..24 {
        let name = format!("cas/k{n:02}");
        let body = Bytes::from(vec![b'x'; n + 1]);
        store.put_new(&key(&name), body, None).await.unwrap();
    }
    put(&store, "other/x", b"o").await;
    let cold = Bytes::from_static(b"cold");
    h.cold_mem.put("cas/k24", cold).await.unwrap();
    let prefix = KeyPrefix::new("cas/").unwrap();
    let size = PageSize::new(10).unwrap();
    let mut listed = Vec::new();
    let mut pages = 0;
    let mut token: Option<ListToken> = None;
    loop {
        let page = store.list(&prefix, token.as_ref(), size).await.unwrap();
        pages += 1;
        listed.extend(page.objects);
        token = page.next;
        if token.is_none() {
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(listed.len(), 25);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    let want: Vec<String> = (0..25).map(|n| format!("cas/k{n:02}")).collect();
    assert_eq!(names, want);
    assert_eq!(listed[3].size, 4);
    assert_eq!(listed[24].size, 4);
    let after = ListToken::after(&key("cas/k20"));
    let most = PageSize::MAX;
    let rest = store.list(&prefix, Some(&after), most).await.unwrap();
    assert_eq!(rest.objects.len(), 4);
    assert!(rest.next.is_none());
    let everything = KeyPrefix::new("").unwrap();
    let all = store.list(&everything, None, most).await.unwrap();
    assert_eq!(all.objects.len(), 26);
}

/// Asserts that an object only the cold tier holds is read with the client's ranged reads, and
/// that the first read copies it into the hot tier.
#[tokio::test]
async fn an_object_of_the_cold_tier_is_read() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(CONDITIONAL);
    let body = Bytes::from_static(b"0123456789");
    h.cold_mem.put("cas/cold", body.clone()).await.unwrap();
    assert_eq!(read(&store, "cas/cold", 2, 4).await, "2345");
    assert_eq!(h.hot_mem.get("cas/cold", None).await.unwrap(), body);
}

/// Asserts that an access key the server does not know, a wrong secret and a request without a
/// signature are all 403, with the codes S3 gives, and that nothing is stored for them.
#[tokio::test]
async fn only_the_configured_key_is_served() {
    let h = Harness::start(|_| {}).await;
    let k = key("cas/denied");
    let body = Bytes::from_static(b"x");
    let stranger = h.store_with("AKIDSTRANGER", "tierstore-test-secret", CONDITIONAL);
    let put = stranger.put_new(&k, body.clone(), None).await;
    assert_refused(put, 403, "InvalidAccessKeyId");
    let guesser = h.store_with("AKIDTIERSTORE", "another-secret", CONDITIONAL);
    let put = guesser.put_new(&k, body, None).await;
    assert_refused(put, 403, "SignatureDoesNotMatch");
    let read = guesser.get_range(&k, window(0, 1)).await;
    assert_refused(read, 403, "SignatureDoesNotMatch");
    let url = format!("http://{}/tier-bucket/cas/denied", h.addr);
    let unsigned = reqwest::Client::new().put(url).body("x").send().await;
    let unsigned = unsigned.unwrap();
    assert_eq!(unsigned.status(), 403);
    let text = unsigned.text().await.unwrap();
    assert!(text.contains("<Code>AccessDenied</Code>"), "{text}");
    assert!(h.hot_mem.get("cas/denied", None).await.is_err());
    let waiting = h.store(CONDITIONAL);
    waiting.wait_answering(Duration::from_secs(5)).await.unwrap();
}

/// Asserts that a put the store has no room for, in either tier, is answered 503 `SlowDown`,
/// that a store that does not answer is 503 `ServiceUnavailable`, and that the next put, after
/// the trouble, succeeds.
#[tokio::test]
async fn a_full_store_answers_slow_down() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(CONDITIONAL);
    let no_room_hot = Error::Full("hot".to_owned());
    let no_room_cold = Error::Full("cold".to_owned());
    h.hot.fail_next_with(Op::Put, no_room_hot);
    h.cold.fail_next_with(Op::Put, no_room_cold);
    let body = Bytes::from_static(b"x");
    let full = store.put_new(&key("cas/full"), body.clone(), None).await;
    assert_refused(full, 503, "SlowDown");
    h.cold.fail_next(Op::Head, 1);
    let down = store.put_new(&key("cas/down"), body, None).await;
    assert_refused(down, 503, "ServiceUnavailable");
    put(&store, "cas/after", b"x").await;
}

/// Asserts that a key under the prefixes the proxy keeps for itself cannot be written, read or
/// deleted by the client: 403 `AccessDenied`.
#[tokio::test]
async fn the_keys_of_the_proxy_are_out_of_reach() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(CONDITIONAL);
    for name in ["_journal/p1/00000000000000000001", "_tiering/lease"] {
        let k = key(name);
        let body = Bytes::from_static(b"x");
        let put = store.put_new(&k, body, None).await;
        assert_refused(put, 403, "AccessDenied");
        let read = store.get_range(&k, window(0, 1)).await;
        assert_refused(read, 403, "AccessDenied");
        let delete = store.delete(&k).await;
        assert_refused(delete, 403, "AccessDenied");
    }
}

/// `len` bytes that differ from their neighbours, so a part sent twice, lost or out of order
/// reads back wrong.
fn pattern(len: usize) -> Bytes {
    let bytes: Vec<u8> = (0..len)
        .map(|i| (i % 251) as u8 ^ (i >> 16) as u8)
        .collect();
    Bytes::from(bytes)
}

/// The bytes in pieces of a size that does not divide a part.
fn pieces(bytes: &Bytes) -> BodyStream {
    const PIECE: usize = (1 << 20) + 7;
    let chunks: Vec<Result<Bytes, ObjectStoreError>> = (0..bytes.len())
        .step_by(PIECE)
        .map(|at| Ok(bytes.slice(at..(at + PIECE).min(bytes.len()))))
        .collect();
    stream::iter(chunks).boxed()
}

/// Asserts that an object over the size of one put goes up as a multipart upload with the
/// client's own parts, is stored whole in the hot tier and reads back byte for byte, and that a
/// second upload of the key fails at its completion with `AlreadyExists` and leaves the first
/// object.
#[tokio::test]
async fn a_large_object_goes_up_in_parts_and_reads_back() {
    let h = Harness::start(|_| {}).await;
    let store = h.store(CONDITIONAL);
    let bytes = pattern(MULTIPART_THRESHOLD as usize + 1000);
    let k = key("cas/large");
    let len = bytes.len() as u64;
    store.put_stream(&k, len, pieces(&bytes)).await.unwrap();
    assert_eq!(h.hot_mem.get("cas/large", None).await.unwrap(), bytes);
    assert_eq!(store.get_range(&k, window(0, len)).await.unwrap(), bytes);
    let tail = store.get_range(&k, window(len - 10, 10)).await.unwrap();
    assert_eq!(tail, bytes.slice(bytes.len() - 10..));
    let again = store.put_stream(&k, len, pieces(&bytes)).await;
    assert!(matches!(again, Err(ObjectStoreError::AlreadyExists(_))));
    assert_eq!(h.hot_mem.get("cas/large", None).await.unwrap(), bytes);
}
