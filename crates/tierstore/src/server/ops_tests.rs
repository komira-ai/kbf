//! The S3 operations, through `Server::handle`, over a proxy with in-memory tiers.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};
use sha2::{Digest, Sha256};

use crate::{Backend, Error, Op};

use super::auth::sign;
use super::ops::{Window, window};
use super::testing::{BUCKET, Reply, Rig, texts};

const MEBIBYTE: u64 = 1 << 20;

fn tag_of(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The keys of a listing answer.
fn keys(body: &str) -> Vec<String> {
    texts(body, "Key")
}

/// Asserts that an object put through the server is in the hot tier, is read back whole with
/// its tag, time and length, is described by a head, and is gone after a delete, which
/// succeeds for a key that is not there; and that every answer has a request id of its own.
#[tokio::test(start_paused = true)]
async fn an_object_is_put_read_described_and_deleted() {
    let rig = Rig::new(MEBIBYTE).await;
    let put = rig.put("cas/a", b"hello world").await;
    assert_eq!(put.status, StatusCode::OK);
    let stored = rig.hot_mem.get("cas/a", None).await.unwrap();
    assert_eq!(stored, Bytes::from_static(b"hello world"));
    let got = rig.get("cas/a", None).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body, "hello world");
    let tag = tag_of(b"hello world");
    assert_eq!(got.header("etag"), format!("\"{tag}\""));
    let modified = got.header("last-modified");
    assert_eq!(modified, "Fri, 15 Jan 2027 08:00:00 GMT");
    assert_eq!(got.header("accept-ranges"), "bytes");
    assert_eq!(got.header("content-type"), "application/octet-stream");
    let head = rig.call("HEAD", "cas/a", b"").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.body, "");
    assert_eq!(head.header("content-length"), "11");
    assert_eq!(head.header("etag"), got.header("etag"));
    let deleted = rig.call("DELETE", "cas/a", b"").await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = rig.get("cas/a", None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert_eq!(gone.code(), "NoSuchKey");
    assert_eq!(gone.header("content-type"), "application/xml");
    let again = rig.call("DELETE", "cas/a", b"").await;
    assert_eq!(again.status, StatusCode::NO_CONTENT);
    let absent = rig.call("HEAD", "cas/a", b"").await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    assert_eq!(put.header("x-amz-request-id").len(), 16);
    let first = gone.header("x-amz-request-id");
    let second = again.header("x-amz-request-id");
    assert_ne!(first, second);
}

/// Asserts that a put only creates: a second put of a key is `PreconditionFailed` with or
/// without `If-None-Match: *`, the first object is kept, and a put without the header of a key
/// that is not there creates it.
#[tokio::test(start_paused = true)]
async fn a_put_creates_and_never_replaces() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("k", b"one").await.status, StatusCode::OK);
    let again = rig.put("k", b"two").await;
    assert_eq!(again.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(again.code(), "PreconditionFailed");
    let plain = rig.call("PUT", "k", b"three").await;
    assert_eq!(plain.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(rig.get("k", None).await.body, "one");
    let fresh = rig.call("PUT", "j", b"plain").await;
    assert_eq!(fresh.status, StatusCode::OK);
    assert_eq!(rig.get("j", None).await.body, "plain");
}

/// Asserts that a put with `If-Match` is refused with `PreconditionFailed` for another tag,
/// `NoSuchKey` for a key that is not there, and `NotImplemented` for the tag the object has
/// (it cannot be replaced), and that an `If-None-Match` other than `*` is `NotImplemented`;
/// the object is never changed.
#[tokio::test(start_paused = true)]
async fn a_put_with_if_match_never_replaces() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("k", b"one").await;
    let (uri, absent) = (format!("/{BUCKET}/k"), format!("/{BUCKET}/absent"));
    let wrong = [("if-match", "\"nope\"")];
    let other = rig.send("PUT", &uri, &wrong, b"two").await;
    assert_eq!(other.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(other.code(), "PreconditionFailed");
    let own = tag_of(b"one");
    let quoted = format!("\"{own}\"");
    let right = [("if-match", quoted.as_str())];
    let same = rig.send("PUT", &uri, &right, b"two").await;
    assert_eq!(same.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(same.code(), "NotImplemented");
    let unquoted = [("if-match", own.as_str())];
    let bare = rig.send("PUT", &uri, &unquoted, b"two").await;
    assert_eq!(bare.status, StatusCode::NOT_IMPLEMENTED);
    let anything = [("if-match", "\"x\"")];
    let missing = rig.send("PUT", &absent, &anything, b"two").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "NoSuchKey");
    let strange = [("if-none-match", "\"x\"")];
    let odd = rig.send("PUT", &uri, &strange, b"two").await;
    assert_eq!(odd.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(rig.get("k", None).await.body, "one");
}

/// Asserts what a `Range` header asks for, of an object of ten bytes: single ranges are cut at
/// the end, suffixes are the last bytes (all of it if longer), and what is not one valid range
/// is the whole object.
#[test]
fn a_range_header_means_what_s3_says() {
    let part = |start, len| Window::Part { start, len };
    assert_eq!(window(None, 10), Window::Whole);
    assert_eq!(window(Some("bytes=0-4"), 10), part(0, 5));
    assert_eq!(window(Some("bytes=8-100"), 10), part(8, 2));
    assert_eq!(window(Some("bytes=3-"), 10), part(3, 7));
    assert_eq!(window(Some("bytes=-4"), 10), part(6, 4));
    assert_eq!(window(Some("bytes=-40"), 10), part(0, 10));
    assert_eq!(window(Some("bytes=10-"), 10), Window::Unsatisfiable);
    assert_eq!(window(Some("bytes=-0"), 10), Window::Unsatisfiable);
    assert_eq!(window(Some("bytes=0-0"), 0), Window::Unsatisfiable);
    let not_ranges = [
        "bytes=5-2",
        "bytes=-",
        "bytes=5",
        "items=0-1",
        "bytes=a-b",
        "bytes=0-1,3-4",
    ];
    for whole in not_ranges {
        assert_eq!(window(Some(whole), 10), Window::Whole, "{whole}");
    }
}

/// Asserts that a ranged get answers 206 with the bytes and an exact `Content-Range`, that what
/// is not a valid single range is answered with the whole object, and that a range that starts
/// at the end is `InvalidRange` with the size.
#[tokio::test(start_paused = true)]
async fn a_ranged_get_answers_the_bytes_and_the_range() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("k", b"0123456789").await;
    let partial = StatusCode::PARTIAL_CONTENT;
    let cases = [
        ("bytes=2-4", partial, "234", "bytes 2-4/10"),
        ("bytes=8-100", partial, "89", "bytes 8-9/10"),
        ("bytes=7-", partial, "789", "bytes 7-9/10"),
        ("bytes=-3", partial, "789", "bytes 7-9/10"),
        ("bytes=-100", partial, "0123456789", "bytes 0-9/10"),
        ("bytes=0-0", partial, "0", "bytes 0-0/10"),
        ("bytes=5-2", StatusCode::OK, "0123456789", ""),
        ("bytes=0-1,3-4", StatusCode::OK, "0123456789", ""),
    ];
    for (range, status, body, content_range) in cases {
        let reply = rig.get("k", Some(range)).await;
        assert_eq!(reply.status, status, "{range}");
        assert_eq!(reply.body, body, "{range}");
        assert_eq!(reply.header("content-range"), content_range, "{range}");
    }
    let past = rig.get("k", Some("bytes=10-")).await;
    assert_eq!(past.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(past.code(), "InvalidRange");
    assert_eq!(past.header("content-range"), "bytes */10");
    rig.put("empty", b"").await;
    assert_eq!(rig.get("empty", None).await.body, "");
    let empty = rig.get("empty", Some("bytes=0-0")).await;
    assert_eq!(empty.status, StatusCode::RANGE_NOT_SATISFIABLE);
}

/// Asserts that an object only the cold tier holds is described by a head without being copied,
/// and read, whole or by range, with the same answers as one in the hot tier; the read copies
/// it into the hot tier.
#[tokio::test(start_paused = true)]
async fn an_object_of_the_cold_tier_is_read_and_copied_back() {
    let rig = Rig::new(MEBIBYTE).await;
    let body = Bytes::from_static(b"0123456789");
    rig.cold_mem.put("cold", body.clone()).await.unwrap();
    let head = rig.call("HEAD", "cold", b"").await;
    assert_eq!(head.header("content-length"), "10");
    assert!(rig.hot_mem.get("cold", None).await.is_err());
    let part = rig.get("cold", Some("bytes=2-4")).await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.body, "234");
    assert_eq!(part.header("content-range"), "bytes 2-4/10");
    let copy = rig.hot_mem.get("cold", None).await.unwrap();
    assert_eq!(copy, body);
}

/// Asserts that no call can name a key under `_journal/` or `_tiering/`: get, head, put,
/// delete and the multipart calls are `AccessDenied`, and a listing leaves those keys out
/// though the hot tier holds them.
#[tokio::test(start_paused = true)]
async fn the_keys_of_the_proxy_are_out_of_reach() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("k", b"v").await;
    assert!(rig.hot_mem.get("_tiering/proxies/p1", None).await.is_ok());
    for key in ["_journal/p1/00000000000000000001", "_tiering/lease"] {
        for method in ["GET", "HEAD", "PUT", "DELETE"] {
            let reply = rig.call(method, key, b"x").await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {key}");
        }
        let start = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(start.status, StatusCode::FORBIDDEN);
        let part_uri = format!("{key}?partNumber=1&uploadId=u");
        let part = rig.call("PUT", &part_uri, b"x").await;
        assert_eq!(part.status, StatusCode::FORBIDDEN);
        let done_uri = format!("{key}?uploadId=u");
        let done = rig.call("POST", &done_uri, b"x").await;
        assert_eq!(done.status, StatusCode::FORBIDDEN);
    }
    let reply = rig.call("GET", "_tiering/lease", b"").await;
    assert_eq!(reply.code(), "AccessDenied");
    assert_eq!(keys(&rig.list("").await.body), ["k"]);
}

async fn put_all(rig: &Rig, names: &[&str]) {
    for name in names {
        assert_eq!(rig.put(name, b"x").await.status, StatusCode::OK);
    }
}

/// Asserts that a listing in pages of two follows the continuation tokens through all the
/// keys with the prefix, once each in order, and that the last page is not truncated.
#[tokio::test(start_paused = true)]
async fn a_listing_pages_through_the_keys_with_the_prefix() {
    let rig = Rig::new(MEBIBYTE).await;
    put_all(&rig, &["k1", "k2", "k3", "k4", "k5", "other"]).await;
    let mut seen = Vec::new();
    let mut pages = 0;
    let mut query = "prefix=k&max-keys=2".to_owned();
    loop {
        let page = rig.list(&query).await;
        assert_eq!(page.status, StatusCode::OK);
        assert_eq!(texts(&page.body, "MaxKeys"), ["2"]);
        seen.extend(keys(&page.body));
        pages += 1;
        let next = texts(&page.body, "NextContinuationToken");
        let Some(token) = next.first() else {
            assert_eq!(texts(&page.body, "IsTruncated"), ["false"]);
            break;
        };
        assert_eq!(texts(&page.body, "IsTruncated"), ["true"]);
        query = format!("prefix=k&max-keys=2&continuation-token={token}");
    }
    assert_eq!(seen, ["k1", "k2", "k3", "k4", "k5"]);
    assert_eq!(pages, 3);
}

/// Asserts the edges of a page: exactly as many keys as the page holds is not truncated, one
/// more is; `start-after` starts after a key that need not exist; a continuation token wins
/// over `start-after`; a listing merges the keys of both tiers; and a size of more than 1000
/// is 1000.
#[tokio::test(start_paused = true)]
async fn a_listing_starts_where_it_is_told_and_merges_the_tiers() {
    let rig = Rig::new(MEBIBYTE).await;
    put_all(&rig, &["k1", "k2", "k3"]).await;
    let cold = Bytes::from_static(b"c");
    rig.cold_mem.put("k0", cold).await.unwrap();
    let all = rig.list("max-keys=4").await;
    assert_eq!(keys(&all.body), ["k0", "k1", "k2", "k3"]);
    assert_eq!(texts(&all.body, "IsTruncated"), ["false"]);
    let short = rig.list("max-keys=3").await;
    assert_eq!(keys(&short.body), ["k0", "k1", "k2"]);
    assert_eq!(texts(&short.body, "IsTruncated"), ["true"]);
    let token = hex::encode("k2");
    let next = texts(&short.body, "NextContinuationToken");
    assert_eq!(next, [token.as_str()]);
    let after = rig.list("start-after=k1").await;
    assert_eq!(keys(&after.body), ["k2", "k3"]);
    let between = rig.list("start-after=k1x").await;
    assert_eq!(keys(&between.body), ["k2", "k3"]);
    let both_query = format!("start-after=k0&continuation-token={token}");
    let both = rig.list(&both_query).await;
    assert_eq!(keys(&both.body), ["k3"]);
    let large = rig.list("max-keys=5000").await;
    assert_eq!(texts(&large.body, "MaxKeys"), ["1000"]);
    let empty = rig.list("prefix=zzz").await;
    assert_eq!(texts(&empty.body, "KeyCount"), ["0"]);
}

/// Asserts that a listing is refused when its size is not a positive whole number or its token
/// is not one the server gave (not hex, or hex of bytes that are not text), and that anything
/// but ListObjectsV2 or with a parameter this server does not know (`delimiter`) is
/// `NotImplemented`.
#[tokio::test(start_paused = true)]
async fn a_listing_with_a_bad_argument_is_refused() {
    let rig = Rig::new(MEBIBYTE).await;
    let bad = [
        "max-keys=0",
        "max-keys=many",
        "continuation-token=zz",
        "continuation-token=ff",
    ];
    for query in bad {
        let reply = rig.list(query).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(reply.code(), "InvalidArgument", "{query}");
    }
    let unsupported = rig.list("delimiter=%2F").await;
    assert_eq!(unsupported.status, StatusCode::NOT_IMPLEMENTED);
    let bucket = format!("/{BUCKET}");
    let old = rig.send("GET", &bucket, &[], b"").await;
    assert_eq!(old.status, StatusCode::NOT_IMPLEMENTED);
    let version_one = format!("/{BUCKET}?list-type=1");
    let other = rig.send("GET", &version_one, &[], b"").await;
    assert_eq!(other.status, StatusCode::NOT_IMPLEMENTED);
}

/// Asserts that creating and heading the served bucket succeed, another bucket is
/// `NoSuchBucket`, and the calls this server does not do are `NotImplemented`: the service
/// root, another method, a subresource of an object, a multipart parameter on a plain read,
/// a post with no upload, a post with a part number and no upload id, and a delete of the
/// bucket.
#[tokio::test(start_paused = true)]
async fn the_bucket_calls_and_the_calls_that_are_not_done() {
    let rig = Rig::new(MEBIBYTE).await;
    let bucket = format!("/{BUCKET}");
    let created = rig.send("PUT", &bucket, &[], b"").await;
    assert_eq!(created.status, StatusCode::OK);
    let described = rig.send("HEAD", &bucket, &[], b"").await;
    assert_eq!(described.status, StatusCode::OK);
    let other = rig.send("GET", "/other-bucket/k", &[], b"").await;
    assert_eq!(other.status, StatusCode::NOT_FOUND);
    assert_eq!(other.code(), "NoSuchBucket");
    let patch = format!("{bucket}/k");
    let not_done = [
        rig.send("GET", "/", &[], b"").await,
        rig.send("PATCH", &patch, &[], b"").await,
        rig.call("GET", "k?acl", b"").await,
        rig.call("GET", "k?uploadId=u", b"").await,
        rig.call("POST", "k", b"").await,
        rig.call("POST", "k?partNumber=1", b"").await,
        rig.send("DELETE", &bucket, &[], b"").await,
    ];
    for reply in not_done {
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(reply.code(), "NotImplemented");
    }
}

/// Asserts how the store's failures are answered: no room in either tier is `SlowDown` (503),
/// a tier that does not answer is `ServiceUnavailable` (503), and any other failure is
/// `InternalError` (500) with its text.
#[tokio::test(start_paused = true)]
async fn the_failures_of_the_store_are_answered_with_their_codes() {
    let rig = Rig::new(MEBIBYTE).await;
    let no_room_hot = Error::Full("hot".to_owned());
    let no_room_cold = Error::Full("cold".to_owned());
    rig.hot.fail_next_with(Op::Put, no_room_hot);
    rig.cold.fail_next_with(Op::Put, no_room_cold);
    let full = rig.put("a", b"x").await;
    assert_eq!(full.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(full.code(), "SlowDown");
    rig.cold.fail_next(Op::Head, 1);
    let down = rig.put("b", b"x").await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.code(), "ServiceUnavailable");
    rig.put("c", b"x").await;
    let boom = Error::Other("boom".to_owned());
    rig.hot.fail_next_with(Op::Get, boom);
    let failed = rig.get("c", None).await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failed.code(), "InternalError");
    assert!(texts(&failed.body, "Message").concat().contains("boom"));
}

/// Asserts that a request whose body is not the one the signature covers is refused before
/// anything is stored, and that a request with no signature is `AccessDenied`.
#[tokio::test(start_paused = true)]
async fn a_forged_body_and_a_missing_signature_are_refused() {
    let rig = Rig::new(MEBIBYTE).await;
    let uri = format!("/{BUCKET}/k");
    let (parts, _) = sign(&rig.config, "PUT", &uri, &[], b"abc").into_parts();
    let forged = Request::from_parts(parts, Full::new(Bytes::from_static(b"abd")));
    let reply = rig.send_raw(forged).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "XAmzContentSHA256Mismatch");
    let head = rig.call("HEAD", "k", b"").await;
    assert_eq!(head.status, StatusCode::NOT_FOUND);
    let builder = Request::builder().method("GET").uri(uri);
    let unsigned = builder.body(Full::new(Bytes::new())).unwrap();
    let refused = rig.send_raw(unsigned).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.code(), "AccessDenied");
}

/// Asserts that a body of the limit is taken and a longer one is `EntityTooLarge` (413).
#[tokio::test(start_paused = true)]
async fn a_body_over_the_limit_is_refused() {
    let rig = Rig::with(MEBIBYTE, |config| config.max_request_bytes = 4).await;
    assert_eq!(rig.put("fits", b"1234").await.status, StatusCode::OK);
    let over = rig.put("over", b"12345").await;
    assert_eq!(over.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(over.code(), "EntityTooLarge");
}

/// Starts a multipart upload of `key` and gives its id.
async fn start(rig: &Rig, key: &str) -> String {
    let uri = format!("{key}?uploads");
    let started = rig.call("POST", &uri, b"").await;
    assert_eq!(started.status, StatusCode::OK);
    texts(&started.body, "UploadId").concat()
}

/// Uploads a part.
async fn upload(rig: &Rig, key: &str, id: &str, number: u32, body: &[u8]) -> Reply {
    let query = format!("{key}?partNumber={number}&uploadId={id}");
    rig.call("PUT", &query, body).await
}

/// Completes the upload `id` of `key` with this body.
async fn complete(rig: &Rig, key: &str, id: &str, body: &str) -> Reply {
    let uri = format!("{key}?uploadId={id}");
    rig.call("POST", &uri, body.as_bytes()).await
}

/// The body that completes an upload with these parts: a number and an ETag for each.
fn completion(parts: &[(u32, &str)]) -> String {
    let mut xml = "<CompleteMultipartUpload>".to_owned();
    for (number, etag) in parts {
        let part = format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>");
        xml.push_str(&part);
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

/// Asserts that an upload in parts is put together: each part answers its ETag, the completion
/// stores the parts joined in the order named, the answer names the key, the object reads back
/// whole, and the server holds no part after.
#[tokio::test(start_paused = true)]
async fn a_multipart_upload_is_completed_into_one_object() {
    let rig = Rig::new(MEBIBYTE).await;
    let id = start(&rig, "big").await;
    let two = upload(&rig, "big", &id, 2, b"world").await;
    let one = upload(&rig, "big", &id, 1, b"hello ").await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.header("etag"), format!("\"{}\"", tag_of(b"hello ")));
    assert_eq!(rig.server.uploads.held(), 11);
    let body = completion(&[(1, one.header("etag")), (2, two.header("etag"))]);
    let done = complete(&rig, "big", &id, &body).await;
    assert_eq!(done.status, StatusCode::OK);
    assert_eq!(texts(&done.body, "Key"), ["big"]);
    assert!(texts(&done.body, "ETag").concat().ends_with("-2\""));
    assert_eq!(rig.get("big", None).await.body, "hello world");
    assert_eq!(rig.server.uploads.held(), 0);
    let again = complete(&rig, "big", &id, &body).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    assert_eq!(again.code(), "NoSuchUpload");
}

/// Asserts that a completion that names parts wrongly is refused, and the upload stays open:
/// a part that was not uploaded, an ETag that is not the part's, parts out of order, a body
/// that is not a list of parts, and an empty list.
#[tokio::test(start_paused = true)]
async fn a_completion_that_names_the_parts_wrongly_is_refused() {
    let rig = Rig::new(MEBIBYTE).await;
    let id = start(&rig, "big").await;
    let one = upload(&rig, "big", &id, 1, b"a").await;
    let two = upload(&rig, "big", &id, 2, b"b").await;
    let (e1, e2) = (one.header("etag"), two.header("etag"));
    let cases = [
        (completion(&[(3, e1)]), "InvalidPart"),
        (completion(&[(1, "\"nope\"")]), "InvalidPart"),
        (completion(&[(2, e2), (1, e1)]), "InvalidPartOrder"),
        (completion(&[(1, e1), (1, e1)]), "InvalidPartOrder"),
        ("not a list".to_owned(), "MalformedXML"),
        (completion(&[]), "MalformedXML"),
    ];
    for (body, code) in cases {
        let reply = complete(&rig, "big", &id, &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{code}");
        assert_eq!(reply.code(), code);
    }
    assert_eq!(rig.server.uploads.held(), 2);
    let both = completion(&[(1, e1), (2, e2)]);
    let done = complete(&rig, "big", &id, &both).await;
    assert_eq!(done.status, StatusCode::OK);
}

/// Asserts that the calls on an upload the server does not hold are `NoSuchUpload`: a part,
/// a completion and an abort of an unknown id or of another key, and a part after an abort.
#[tokio::test(start_paused = true)]
async fn an_upload_that_is_not_held_is_no_such_upload() {
    let rig = Rig::new(MEBIBYTE).await;
    let id = start(&rig, "big").await;
    let one = upload(&rig, "big", &id, 1, b"a").await;
    let body = completion(&[(1, one.header("etag"))]);
    let wrong_key = complete(&rig, "other", &id, &body).await;
    assert_eq!(wrong_key.code(), "NoSuchUpload");
    let unknown = complete(&rig, "big", "nope", &body).await;
    assert_eq!(unknown.code(), "NoSuchUpload");
    let no_upload = upload(&rig, "big", "nope", 1, b"a").await;
    assert_eq!(no_upload.code(), "NoSuchUpload");
    let other_key = upload(&rig, "other", &id, 1, b"a").await;
    assert_eq!(other_key.code(), "NoSuchUpload");
    let other_abort = format!("other?uploadId={id}");
    let mismatch = rig.call("DELETE", &other_abort, b"").await;
    assert_eq!(mismatch.status, StatusCode::NOT_FOUND);
    let abort = format!("big?uploadId={id}");
    let aborted = rig.call("DELETE", &abort, b"").await;
    assert_eq!(aborted.status, StatusCode::NO_CONTENT);
    assert_eq!(rig.server.uploads.held(), 0);
    let late = upload(&rig, "big", &id, 2, b"a").await;
    assert_eq!(late.status, StatusCode::NOT_FOUND);
    let twice = rig.call("DELETE", &abort, b"").await;
    assert_eq!(twice.status, StatusCode::NOT_FOUND);
}

/// Asserts that a part needs an upload id and a part number from 1 to 10000, and that a
/// completion that finds the key taken is `PreconditionFailed` and leaves the upload open.
#[tokio::test(start_paused = true)]
async fn a_part_needs_its_arguments_and_a_completion_needs_a_free_key() {
    let rig = Rig::new(MEBIBYTE).await;
    let id = start(&rig, "big").await;
    let queries = [
        "big?partNumber=1".to_owned(),
        format!("big?uploadId={id}"),
        format!("big?partNumber=0&uploadId={id}"),
        format!("big?partNumber=10001&uploadId={id}"),
        format!("big?partNumber=one&uploadId={id}"),
    ];
    for query in queries {
        let reply = rig.call("PUT", &query, b"a").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(reply.code(), "InvalidArgument", "{query}");
    }
    let last = upload(&rig, "big", &id, 10_000, b"a").await;
    assert_eq!(last.status, StatusCode::OK);
    rig.put("big", b"taken").await;
    let body = completion(&[(10_000, last.header("etag"))]);
    let refused = complete(&rig, "big", &id, &body).await;
    assert_eq!(refused.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(rig.server.uploads.held(), 1);
    assert_eq!(rig.get("big", None).await.body, "taken");
}

/// Asserts that the parts held are limited: a part that would take the parts of all the uploads
/// past the limit is `SlowDown` (it can be sent again when other uploads are done), a part that
/// the limit never allows, because the parts of its upload alone would pass it, is 400
/// `EntityTooLarge`, and one that fits is taken, also in place of another part.
#[tokio::test(start_paused = true)]
async fn the_parts_held_are_limited() {
    let rig = Rig::with(MEBIBYTE, |config| config.max_staged_bytes = 4).await;
    let id = start(&rig, "big").await;
    let too_large = upload(&rig, "big", &id, 1, b"12345").await;
    assert_eq!(too_large.status, StatusCode::BAD_REQUEST);
    assert_eq!(too_large.code(), "EntityTooLarge");
    let fits = upload(&rig, "big", &id, 1, b"123").await;
    assert_eq!(fits.status, StatusCode::OK);
    let other = start(&rig, "other").await;
    let crowded = upload(&rig, "other", &other, 1, b"12").await;
    assert_eq!(crowded.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(crowded.code(), "SlowDown");
    let replaced = upload(&rig, "big", &id, 1, b"1234").await;
    assert_eq!(replaced.status, StatusCode::OK);
}

/// Asserts that a listing of more keys than the largest page has a second page: 1001 keys are
/// listed as 1000 and a token, and then the last key.
#[tokio::test(start_paused = true)]
async fn a_listing_of_more_than_a_thousand_keys_has_a_second_page() {
    let rig = Rig::new(MEBIBYTE).await;
    for number in 0..1001 {
        let key = format!("k{number:04}");
        rig.cold_mem.put(&key, Bytes::new()).await.unwrap();
    }
    let first = rig.list("").await;
    assert_eq!(keys(&first.body).len(), 1000);
    assert_eq!(texts(&first.body, "IsTruncated"), ["true"]);
    let token = texts(&first.body, "NextContinuationToken").concat();
    let second = rig.list(&format!("continuation-token={token}")).await;
    assert_eq!(keys(&second.body), ["k1000"]);
    assert_eq!(texts(&second.body, "IsTruncated"), ["false"]);
}

/// Reads the object `k` with one header.
async fn read_with(rig: &Rig, method: &str, name: &str, value: &str) -> Reply {
    let uri = format!("/{BUCKET}/k");
    rig.send(method, &uri, &[(name, value)], b"").await
}

/// Asserts the conditions of a read, for a get and for a head: `If-Match` of another tag is
/// `PreconditionFailed`, and of the tag of the object (also among others, and as `*`) is
/// served; `If-None-Match` of the tag of the object (also among others, and as `*`) is 304 with
/// the tag and no body, and of another tag is served; the conditions on the time are
/// `NotImplemented`.
#[tokio::test(start_paused = true)]
async fn a_read_with_conditions_is_served_or_refused_as_s3_does() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("k", b"one").await;
    let own = format!("\"{}\"", tag_of(b"one"));
    let listed = format!("\"x\", {own}");
    for method in ["GET", "HEAD"] {
        let served = [
            ("if-match", own.as_str()),
            ("if-match", "*"),
            ("if-match", listed.as_str()),
            ("if-none-match", "\"x\""),
        ];
        for (name, value) in served {
            let reply = read_with(&rig, method, name, value).await;
            assert_eq!(reply.status, StatusCode::OK, "{method} {name} {value}");
        }
        let wrong = read_with(&rig, method, "if-match", "\"x\"").await;
        assert_eq!(wrong.status, StatusCode::PRECONDITION_FAILED, "{method}");
        assert_eq!(wrong.code(), "PreconditionFailed", "{method}");
        for value in [own.as_str(), "*", listed.as_str()] {
            let same = read_with(&rig, method, "if-none-match", value).await;
            assert_eq!(same.status, StatusCode::NOT_MODIFIED, "{method} {value}");
            assert_eq!(same.header("etag"), own);
            assert_eq!(same.body, "");
        }
        for name in ["if-modified-since", "if-unmodified-since"] {
            let date = "Fri, 15 Jan 2027 08:00:00 GMT";
            let timed = read_with(&rig, method, name, date).await;
            assert_eq!(timed.status, StatusCode::NOT_IMPLEMENTED, "{method} {name}");
        }
    }
}

/// Asserts that a copy is refused and creates nothing: a put and an upload of a part with
/// `x-amz-copy-source` are `NotImplemented`, and the key is not taken.
#[tokio::test(start_paused = true)]
async fn a_copy_is_not_implemented_and_creates_nothing() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("from", b"one").await;
    let from = format!("/{BUCKET}/from");
    let source = [("x-amz-copy-source", from.as_str())];
    let uri = format!("/{BUCKET}/to");
    let copy = rig.send("PUT", &uri, &source, b"").await;
    assert_eq!(copy.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(copy.code(), "NotImplemented");
    let id = start(&rig, "to").await;
    let part_uri = format!("{uri}?partNumber=1&uploadId={id}");
    let part = rig.send("PUT", &part_uri, &source, b"").await;
    assert_eq!(part.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(rig.server.uploads.held(), 0);
    let absent = rig.call("HEAD", "to", b"").await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
}

/// Sends a signed request for an object of the bucket and drops it after a second, as the
/// server does when the client goes away before the answer.
async fn dropped(rig: &Rig, method: &str, key: &str, body: &[u8]) {
    let uri = format!("/{BUCKET}/{key}");
    let request = sign(&rig.config, method, &uri, &[], body);
    let handling = rig.server.handle(request);
    let waited = tokio::time::timeout(Duration::from_secs(1), handling).await;
    assert!(waited.is_err());
}

/// Asserts that a call that changes the store is done to its end when its request is dropped
/// before the store has answered: a put leaves the object and a journal entry, a delete leaves
/// the key deleted, and the completion of an upload leaves the object.
#[tokio::test(start_paused = true)]
async fn a_request_that_is_dropped_still_changes_the_store() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("gone", b"x").await;
    let id = start(&rig, "whole").await;
    let part = upload(&rig, "whole", &id, 1, b"a").await;
    let body = completion(&[(1, part.header("etag"))]);
    let before = rig.hot_mem.list("_journal/p1/").await.unwrap().len();
    rig.hot.set_delay(Op::Put, Some(Duration::from_secs(10)));
    rig.hot.set_delay(Op::Delete, Some(Duration::from_secs(10)));
    dropped(&rig, "PUT", "fresh", b"y").await;
    dropped(&rig, "DELETE", "gone", b"").await;
    let complete_key = format!("whole?uploadId={id}");
    dropped(&rig, "POST", &complete_key, body.as_bytes()).await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(rig.hot_mem.get("fresh", None).await.unwrap(), "y");
    assert!(rig.hot_mem.get("gone", None).await.is_err());
    assert_eq!(rig.hot_mem.get("whole", None).await.unwrap(), "a");
    let after = rig.hot_mem.list("_journal/p1/").await.unwrap().len();
    assert!(after > before);
}

/// Asserts that an upload that gets no part for the time to live is dropped.
#[tokio::test(start_paused = true)]
async fn an_upload_is_dropped_after_the_time_to_live() {
    let ttl = Duration::from_secs(60);
    let rig = Rig::with(MEBIBYTE, |config| config.upload_ttl = ttl).await;
    let id = start(&rig, "big").await;
    tokio::time::advance(Duration::from_secs(61)).await;
    let late = upload(&rig, "big", &id, 1, b"a").await;
    assert_eq!(late.status, StatusCode::NOT_FOUND);
    assert_eq!(late.code(), "NoSuchUpload");
}
