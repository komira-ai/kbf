//! What a read-only access key may do through `Server::handle`: read, and nothing that writes.

use hyper::StatusCode;

use crate::{Backend, Listed, Op};

use super::AccessKey;
use super::ops_tests::{completion, start, upload};
use super::testing::{BUCKET, KEY_ID, Reply, Rig, SECRET, texts};

const MEBIBYTE: u64 = 1 << 20;

/// The id and the secret of the read-only key of the rig.
const READER: (&str, &str) = ("AKIDREADER", "reader-secret");

/// The header of a create-only put.
const CREATE: [(&str, &str); 1] = [("if-none-match", "*")];

/// A call to try: its method, its URI and its headers.
type Case<'a> = (&'a str, String, &'a [(&'a str, &'a str)]);

/// A rig whose server has the read-only key `READER` besides its full key.
async fn rig() -> Rig {
    let keys = vec![AccessKey::new(READER.0, READER.1)];
    let made = Rig::with(MEBIBYTE, |config| config.read_only_keys = keys);
    made.await
}

/// The URI of an object of the bucket (`key` may carry a query).
fn object(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

/// The URI that completes or aborts the upload `id` of the object `big`.
fn upload_uri(id: &str) -> String {
    format!("{}?uploadId={id}", object("big"))
}

/// Sends a request signed with the read-only key.
async fn as_reader(
    rig: &Rig,
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let sent = rig.send_as(READER, method, uri, extra, body);
    sent.await
}

/// What the tiers hold, and the number of calls that deletes and uploads made to the hot tier
/// and to the cold tier, in turn.
async fn state(rig: &Rig) -> (Vec<Listed>, Vec<Listed>, Vec<u64>) {
    let hot = rig.hot_mem.list("").await.unwrap();
    let cold = rig.cold_mem.list("").await.unwrap();
    let ops = [
        Op::Delete,
        Op::CreateUpload,
        Op::UploadPart,
        Op::CompleteUpload,
        Op::AbortUpload,
    ];
    let both = |op: &Op| [rig.hot.calls(*op), rig.cold.calls(*op)];
    let calls = ops.iter().flat_map(both).collect();
    (hot, cold, calls)
}

/// Asserts that a read-only key gets an object whole and by range, heads it, lists the bucket
/// and heads the bucket, and that a read of a key that is not there is `NoSuchKey`, as for the
/// full key: the role holds back the calls that write, and not the reads.
#[tokio::test(start_paused = true)]
async fn a_read_only_key_gets_heads_and_lists() {
    let rig = rig().await;
    rig.put("cas/a", b"hello world").await;
    let uri = object("cas/a");
    let got = as_reader(&rig, "GET", &uri, &[], b"").await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body, "hello world");
    let by_full_key = rig.get("cas/a", None).await;
    assert_eq!(got.header("etag"), by_full_key.header("etag"));
    let from_six = [("range", "bytes=6-")];
    let part = as_reader(&rig, "GET", &uri, &from_six, b"").await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.body, "world");
    let head = as_reader(&rig, "HEAD", &uri, &[], b"").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "11");
    let missing = object("absent");
    let absent = as_reader(&rig, "GET", &missing, &[], b"").await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    assert_eq!(absent.code(), "NoSuchKey");
    let listing = format!("/{BUCKET}?list-type=2&prefix=cas");
    let listed = as_reader(&rig, "GET", &listing, &[], b"").await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(texts(&listed.body, "Key"), ["cas/a"]);
    let bucket_uri = format!("/{BUCKET}");
    let bucket = as_reader(&rig, "HEAD", &bucket_uri, &[], b"").await;
    assert_eq!(bucket.status, StatusCode::OK);
}

/// Asserts that every call that writes is `AccessDenied` (403, with the S3 error document) for
/// a read-only key, and changes nothing: a put with and without `If-None-Match`, a put with
/// `If-Match`, CreateBucket, a delete, the start of an upload, a part, the completion and the
/// abort of an upload. The tiers hold what they held and no delete or upload call reached
/// either, and the upload that was open is still open for the full key to complete.
#[tokio::test(start_paused = true)]
async fn a_read_only_key_is_denied_every_write_and_changes_nothing() {
    let rig = rig().await;
    rig.put("kept", b"original").await;
    let id = start(&rig, "big").await;
    let one = upload(&rig, "big", &id, 1, b"part one").await;
    let before = state(&rig).await;
    let part = format!("{}?partNumber=2&uploadId={id}", object("big"));
    let cases: [Case<'_>; 9] = [
        ("PUT", object("new"), &CREATE),
        ("PUT", object("new"), &[]),
        ("PUT", object("kept"), &[("if-match", "\"0000\"")]),
        ("PUT", format!("/{BUCKET}"), &[]),
        ("DELETE", object("kept"), &[]),
        ("POST", format!("{}?uploads", object("big")), &[]),
        ("PUT", part, &[]),
        ("POST", upload_uri(&id), &[]),
        ("DELETE", upload_uri(&id), &[]),
    ];
    for (method, uri, extra) in cases {
        let reply = as_reader(&rig, method, &uri, extra, b"changed").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code(), "AccessDenied", "{method} {uri}");
        assert_eq!(reply.header("content-type"), "application/xml");
        assert!(reply.body.contains("may read objects"), "{}", reply.body);
    }
    assert_eq!(state(&rig).await, before);
    let kept = rig.get("kept", None).await;
    assert_eq!(kept.body, "original");
    let new = rig.get("new", None).await;
    assert_eq!(new.status, StatusCode::NOT_FOUND);
    let body = completion(&[(1, one.header("etag"))]);
    let uri = upload_uri(&id);
    let done = rig.send("POST", &uri, &[], body.as_bytes()).await;
    assert_eq!(done.status, StatusCode::OK);
    assert_eq!(rig.cold_mem.get("big", None).await.unwrap(), "part one");
}

/// Asserts that a write by a read-only key is refused before anything of the request is looked
/// at but its signature and its method: a body over the limit, and a checksum header that is
/// not a checksum, are 403 `AccessDenied` and not the 413 and 400 that the full key gets.
#[tokio::test(start_paused = true)]
async fn a_denied_write_is_refused_before_its_body_and_checksums_are_looked_at() {
    let keys = vec![AccessKey::new(READER.0, READER.1)];
    let made = Rig::with(MEBIBYTE, |config| {
        config.read_only_keys = keys;
        config.max_request_bytes = 4;
    });
    let rig = made.await;
    let uri = object("k");
    let over = as_reader(&rig, "PUT", &uri, &CREATE, b"12345").await;
    assert_eq!(over.status, StatusCode::FORBIDDEN);
    assert_eq!(over.code(), "AccessDenied");
    let bad = [("x-amz-checksum-sha256", "not a checksum")];
    let checksum = as_reader(&rig, "PUT", &uri, &bad, b"1234").await;
    assert_eq!(checksum.status, StatusCode::FORBIDDEN);
    assert_eq!(checksum.code(), "AccessDenied");
    let full_over = rig.send("PUT", &uri, &CREATE, b"12345").await;
    assert_eq!(full_over.status, StatusCode::PAYLOAD_TOO_LARGE);
    let full_bad = rig.send("PUT", &uri, &bad, b"1234").await;
    assert_eq!(full_bad.status, StatusCode::BAD_REQUEST);
}

/// Asserts that the full key does everything while a read-only key is on the server: a put, a
/// get, a head, a listing, CreateBucket, a delete, and the start, the part, the completion and
/// the abort of an upload.
#[tokio::test(start_paused = true)]
async fn the_full_key_does_everything_beside_a_read_only_key() {
    let rig = rig().await;
    let put = rig.put("cas/a", b"hello").await;
    assert_eq!(put.status, StatusCode::OK);
    assert_eq!(rig.get("cas/a", None).await.body, "hello");
    let head = rig.call("HEAD", "cas/a", b"").await;
    assert_eq!(head.status, StatusCode::OK);
    let listed = rig.list("prefix=cas").await;
    assert_eq!(texts(&listed.body, "Key"), ["cas/a"]);
    let bucket_uri = format!("/{BUCKET}");
    let created = rig.send("PUT", &bucket_uri, &[], b"").await;
    assert_eq!(created.status, StatusCode::OK);
    let id = start(&rig, "big").await;
    let part = upload(&rig, "big", &id, 1, b"abc").await;
    let body = completion(&[(1, part.header("etag"))]);
    let uri = upload_uri(&id);
    let done = rig.send("POST", &uri, &[], body.as_bytes()).await;
    assert_eq!(done.status, StatusCode::OK);
    let second = start(&rig, "second").await;
    let abort = format!("second?uploadId={second}");
    let aborted = rig.call("DELETE", &abort, b"").await;
    assert_eq!(aborted.status, StatusCode::NO_CONTENT);
    let deleted = rig.call("DELETE", "cas/a", b"").await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = rig.get("cas/a", None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

/// Asserts that a request signed by a key the server does not have is 403 `InvalidAccessKeyId`,
/// and one that names a key and is signed with the secret of another is 403
/// `SignatureDoesNotMatch`, for the full key and for a read-only key.
#[tokio::test(start_paused = true)]
async fn an_unknown_key_and_a_wrong_secret_are_refused() {
    let rig = rig().await;
    let uri = object("k");
    let cases = [
        (("AKIDNOBODY", SECRET), "InvalidAccessKeyId"),
        ((READER.0, SECRET), "SignatureDoesNotMatch"),
        ((KEY_ID, READER.1), "SignatureDoesNotMatch"),
    ];
    for (key, code) in cases {
        let reply = rig.send_as(key, "GET", &uri, &[], b"").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{code}");
        assert_eq!(reply.code(), code);
    }
}

/// Asserts that a read-only key lists the open multipart uploads, by prefix and by page, as the
/// full key does (the listing is a read, as the listing of objects is), and that what it learns
/// from the listing lets it do nothing to an upload: the abort it then tries is 403 and the
/// upload stays open and listed. It catches a role that refuses the listing, and one that lets
/// a key that only reads end an upload.
#[tokio::test(start_paused = true)]
async fn a_read_only_key_lists_open_uploads_and_cannot_end_them() {
    let rig = rig().await;
    let id = start(&rig, "jobs/a").await;
    let other = start(&rig, "other/b").await;
    let all = format!("/{BUCKET}?uploads");
    let by_reader = as_reader(&rig, "GET", &all, &[], b"").await;
    assert_eq!(by_reader.status, StatusCode::OK);
    assert_eq!(texts(&by_reader.body, "Key"), ["jobs/a", "other/b"]);
    assert_eq!(texts(&by_reader.body, "UploadId"), [id.clone(), other]);
    let by_full_key = rig.send("GET", &all, &[], b"").await;
    assert_eq!(by_full_key.body, by_reader.body);

    let under = format!("/{BUCKET}?uploads&prefix=jobs%2F&max-uploads=1");
    let scoped = as_reader(&rig, "GET", &under, &[], b"").await;
    assert_eq!(scoped.status, StatusCode::OK);
    assert_eq!(texts(&scoped.body, "Key"), ["jobs/a"]);
    assert_eq!(texts(&scoped.body, "IsTruncated"), ["false"]);

    let abort = format!("{}?uploadId={id}", object("jobs/a"));
    let refused = as_reader(&rig, "DELETE", &abort, &[], b"").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.code(), "AccessDenied");
    let still = as_reader(&rig, "GET", &all, &[], b"").await;
    assert_eq!(texts(&still.body, "UploadId").len(), 2);
}
