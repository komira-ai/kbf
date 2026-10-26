//! Read-only access keys against the S3 front over loopback: the client of kbf reads, heads and
//! lists with one and is refused every write, requests signed by hand are refused every call
//! that writes with the S3 error document, and the full key does everything beside them.

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_objstore::s3::MULTIPART_THRESHOLD;
use kbf_objstore::{BodyStream, KeyPrefix, ObjectStore, ObjectStoreError, PageSize};
use reqwest::Response;
use tierstore::server::AccessKey;
use tierstore::{Backend, Listed, Op};

use crate::client::{CONDITIONAL, assert_refused, key, put, read, window};
use crate::raw::{call, send, upload_id};
use crate::support::{BUCKET, Harness, KEY_ID, SECRET};

/// The id and the secret of the first read-only key.
const READER: (&str, &str) = ("AKIDREADER", "reader-secret");

/// The id and the secret of the second read-only key.
const OTHER_READER: (&str, &str) = ("AKIDREADER2", "reader-secret-2");

/// The two read-only keys of the servers of these tests.
fn readers() -> Vec<AccessKey> {
    vec![
        AccessKey::new(READER.0, READER.1),
        AccessKey::new(OTHER_READER.0, OTHER_READER.1),
    ]
}

/// A server with the two read-only keys, that tells the time by the clock of the machine.
async fn with_readers() -> Harness {
    let started = Harness::start(|config| config.read_only_keys = readers());
    started.await
}

/// A server with the two read-only keys, whose clock stands at the time that requests signed by
/// hand are signed for.
async fn with_readers_fixed() -> Harness {
    let started = Harness::start_fixed(|config| config.read_only_keys = readers());
    started.await
}

/// A body that goes up in parts, and its length.
fn big() -> (u64, BodyStream) {
    let len = MULTIPART_THRESHOLD + 1;
    let bytes = Bytes::from(vec![9_u8; usize::try_from(len).unwrap()]);
    let parts: BodyStream = stream::iter(vec![Ok(bytes)]).boxed();
    (len, parts)
}

/// Sends a request signed with the key `(id, secret)` for the server at its fixed time.
async fn send_as(
    h: &Harness,
    key: (&str, &str),
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response {
    let signed = h.signed_as(key, method, path, query, headers, body);
    send(h, method, path, query, &signed, body).await
}

/// The path of the object `name` of the bucket, or of the bucket for the name `""`.
fn path_of(name: &str) -> String {
    match name {
        "" => format!("/{BUCKET}"),
        name => format!("/{BUCKET}/{name}"),
    }
}

/// What the tiers hold, and the number of calls that deletes and uploads made to the hot tier
/// and to the cold tier, in turn.
async fn state(h: &Harness) -> (Vec<Listed>, Vec<Listed>, Vec<u64>) {
    let hot = h.hot_mem.list("").await.unwrap();
    let cold = h.cold_mem.list("").await.unwrap();
    let ops = [
        Op::Delete,
        Op::CreateUpload,
        Op::UploadPart,
        Op::CompleteUpload,
        Op::AbortUpload,
    ];
    let both = |op: &Op| [h.hot.calls(*op), h.cold.calls(*op)];
    let calls = ops.iter().flat_map(both).collect();
    (hot, cold, calls)
}

/// Asserts that a call signed by hand with the first read-only key is answered with the 403
/// `AccessDenied` of S3: the error document and a request id. The object is `name`, or the
/// bucket for `""`.
async fn denied(
    h: &Harness,
    method: &str,
    name: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
) {
    let what = format!("{method} {name}");
    let path = path_of(name);
    let response = send_as(h, READER, method, &path, query, headers, b"changed").await;
    assert_eq!(response.status(), 403, "{what}");
    let sent = response.headers();
    assert_eq!(sent["content-type"], "application/xml", "{what}");
    assert_eq!(sent["x-amz-request-id"].len(), 16, "{what}");
    let body = response.text().await.unwrap();
    assert!(body.contains("<Code>AccessDenied</Code>"), "{what}: {body}");
}

/// Asserts that each of two read-only keys, with the client of kbf, reads an object whole and by
/// range, heads it, lists the bucket under a prefix, and gets `NotFound` for a key that is not
/// there, as the full key does.
#[tokio::test]
async fn read_only_keys_read_head_and_list_with_the_client_of_kbf() {
    let h = with_readers().await;
    let full = h.store(CONDITIONAL);
    put(&full, "cas/a", b"hello world").await;
    put(&full, "other/b", b"x").await;
    for (id, secret) in [READER, OTHER_READER] {
        let reader = h.store_with(id, secret, CONDITIONAL);
        assert_eq!(read(&reader, "cas/a", 0, 5).await, "hello", "{id}");
        assert_eq!(read(&reader, "cas/a", 6, 100).await, "world", "{id}");
        let head = reader.head(&key("cas/a")).await.unwrap();
        assert_eq!(head.size, 11, "{id}");
        let prefix = KeyPrefix::new("cas/").unwrap();
        let page = reader.list(&prefix, None, PageSize::MAX).await.unwrap();
        assert_eq!(page.objects.len(), 1, "{id}");
        let absent = reader.get_range(&key("cas/absent"), window(0, 1)).await;
        let not_found = matches!(absent, Err(ObjectStoreError::NotFound(_)));
        assert!(not_found, "{id}");
    }
}

/// Asserts that a read-only key, with the client of kbf, is refused with 403 `AccessDenied`
/// a create-only put of a new key, a put of a key that exists (which the full key is told
/// `AlreadyExists`), a delete, and a put that goes up in parts (nothing of the upload is
/// created), and that the objects are as they were; and that the full key puts in parts on the
/// same server.
#[tokio::test]
async fn the_client_of_kbf_is_refused_writes_with_a_read_only_key() {
    let h = with_readers().await;
    let full = h.store(CONDITIONAL);
    put(&full, "cas/kept", b"original").await;
    let reader = h.store_with(READER.0, READER.1, CONDITIONAL);
    let body = Bytes::from_static(b"changed");
    let created = reader.put_new(&key("cas/new"), body.clone(), None).await;
    assert_refused(created, 403, "AccessDenied");
    let replaced = reader.put_new(&key("cas/kept"), body, None).await;
    assert_refused(replaced, 403, "AccessDenied");
    let deleted = reader.delete(&key("cas/kept")).await;
    assert_refused(deleted, 403, "AccessDenied");
    let (len, parts) = big();
    let in_parts = reader.put_stream(&key("cas/big"), len, parts).await;
    assert_refused(in_parts, 403, "AccessDenied");
    assert_eq!(read(&full, "cas/kept", 0, 8).await, "original");
    assert!(h.hot_mem.get("cas/new", None).await.is_err());
    assert!(h.cold_mem.get("cas/big", None).await.is_err());
    assert_eq!(h.cold.calls(Op::CreateUpload), 0);
    let (len, parts) = big();
    let stored = full.put_stream(&key("cas/big"), len, parts).await;
    stored.unwrap();
    assert_eq!(full.head(&key("cas/big")).await.unwrap().size, len);
    let removed = full.delete(&key("cas/kept")).await;
    removed.unwrap();
}

/// Asserts that requests signed by hand with a read-only key are refused with the S3 error
/// document (403 `AccessDenied`, a request id) for every call that writes: a put with and
/// without `If-None-Match`, a put with `If-Match`, CreateBucket, a delete, the start of an
/// upload, a part, the completion and the abort of an upload; that the tiers hold what they
/// held and no delete or upload call reached either; that the upload that was open is still open
/// for the full key to complete; and that the reads of the same keys are answered.
#[tokio::test]
async fn a_read_only_key_signed_by_hand_is_denied_every_write_and_changes_nothing() {
    let h = with_readers_fixed().await;
    let create = [("if-none-match", "*")];
    call(&h, "PUT", "kept", &[], &create, b"original").await;
    let started = call(&h, "POST", "big", &[("uploads", "")], &[], b"").await;
    let xml = started.text().await.unwrap();
    let id = upload_id(&xml);
    let first = [("partNumber", "1"), ("uploadId", id)];
    let one = call(&h, "PUT", "big", &first, &[], b"part one").await;
    let etag = one.headers()["etag"].to_str().unwrap().to_owned();
    let before = state(&h).await;

    let uploads = [("uploads", "")];
    let second = [("partNumber", "2"), ("uploadId", id)];
    let upload = [("uploadId", id)];
    let replace = [("if-match", "\"0000\"")];
    denied(&h, "PUT", "new", &[], &create).await;
    denied(&h, "PUT", "new", &[], &[]).await;
    denied(&h, "PUT", "kept", &[], &replace).await;
    denied(&h, "PUT", "", &[], &[]).await;
    denied(&h, "DELETE", "kept", &[], &[]).await;
    denied(&h, "POST", "big", &uploads, &[]).await;
    denied(&h, "PUT", "big", &second, &[]).await;
    denied(&h, "POST", "big", &upload, &[]).await;
    denied(&h, "DELETE", "big", &upload, &[]).await;
    assert_eq!(state(&h).await, before);
    assert_eq!(h.hot_mem.get("kept", None).await.unwrap(), "original");
    assert!(h.hot_mem.get("new", None).await.is_err());

    let kept = path_of("kept");
    let got = send_as(&h, READER, "GET", &kept, &[], &[], b"").await;
    assert_eq!(got.status(), 200);
    assert_eq!(got.text().await.unwrap(), "original");
    let head = send_as(&h, OTHER_READER, "HEAD", &kept, &[], &[], b"").await;
    assert_eq!(head.status(), 200);
    let bucket = path_of("");
    let list = [("list-type", "2"), ("prefix", "k")];
    let listed = send_as(&h, READER, "GET", &bucket, &list, &[], b"").await;
    assert_eq!(listed.status(), 200);
    let text = listed.text().await.unwrap();
    assert!(text.contains("<Key>kept</Key>"), "{text}");
    let head_bucket = send_as(&h, READER, "HEAD", &bucket, &[], &[], b"").await;
    assert_eq!(head_bucket.status(), 200);

    let part = format!("<Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part>");
    let xml = format!("<CompleteMultipartUpload>{part}</CompleteMultipartUpload>");
    let done = call(&h, "POST", "big", &upload, &[], xml.as_bytes()).await;
    assert_eq!(done.status(), 200);
    assert_eq!(h.cold_mem.get("big", None).await.unwrap(), "part one");
}

/// Asserts that a request signed by hand with an id that no key has is 403 `InvalidAccessKeyId`,
/// and one that names a key and is signed with the secret of another is 403
/// `SignatureDoesNotMatch`, for the full key and for the read-only keys.
#[tokio::test]
async fn an_unknown_key_id_and_a_crossed_secret_are_refused() {
    let h = with_readers_fixed().await;
    let path = path_of("k");
    let cases = [
        (("AKIDNOBODY", SECRET), "InvalidAccessKeyId"),
        ((READER.0, SECRET), "SignatureDoesNotMatch"),
        ((KEY_ID, READER.1), "SignatureDoesNotMatch"),
        ((OTHER_READER.0, READER.1), "SignatureDoesNotMatch"),
    ];
    for (signer, code) in cases {
        let response = send_as(&h, signer, "GET", &path, &[], &[], b"").await;
        assert_eq!(response.status(), 403, "{code}");
        let text = response.text().await.unwrap();
        assert!(text.contains(&format!("<Code>{code}</Code>")), "{text}");
    }
}

/// Asserts that the full key does everything on a server that has read-only keys: a put, a
/// read, a head, a listing, a delete, CreateBucket, and the start, a part and the abort of an
/// upload.
#[tokio::test]
async fn the_full_key_does_everything_on_a_server_with_read_only_keys() {
    let h = with_readers_fixed().await;
    let create = [("if-none-match", "*")];
    let put = call(&h, "PUT", "cas/a", &[], &create, b"hello").await;
    assert_eq!(put.status(), 200);
    let got = call(&h, "GET", "cas/a", &[], &[], b"").await;
    assert_eq!(got.text().await.unwrap(), "hello");
    let head = call(&h, "HEAD", "cas/a", &[], &[], b"").await;
    assert_eq!(head.status(), 200);
    let list = [("list-type", "2"), ("prefix", "cas")];
    let bucket = path_of("");
    let signed = h.signed("GET", &bucket, &list, &[], b"");
    let listed = send(&h, "GET", &bucket, &list, &signed, b"").await;
    let text = listed.text().await.unwrap();
    assert!(text.contains("<Key>cas/a</Key>"), "{text}");
    let signed = h.signed("PUT", &bucket, &[], &[], b"");
    let created = send(&h, "PUT", &bucket, &[], &signed, b"").await;
    assert_eq!(created.status(), 200);
    let started = call(&h, "POST", "big", &[("uploads", "")], &[], b"").await;
    let xml = started.text().await.unwrap();
    let id = upload_id(&xml);
    let part = [("partNumber", "1"), ("uploadId", id)];
    let one = call(&h, "PUT", "big", &part, &[], b"abc").await;
    assert_eq!(one.status(), 200);
    let abort = [("uploadId", id)];
    let aborted = call(&h, "DELETE", "big", &abort, &[], b"").await;
    assert_eq!(aborted.status(), 204);
    let deleted = call(&h, "DELETE", "cas/a", &[], &[], b"").await;
    assert_eq!(deleted.status(), 204);
    assert!(h.hot_mem.get("cas/a", None).await.is_err());
}
