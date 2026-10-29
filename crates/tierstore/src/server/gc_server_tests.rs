//! What the server does with a request signed by a GC key, through `Server::handle`.

use hyper::StatusCode;

use crate::{Backend, Op};

use super::AccessKey;
use super::ops_tests::completion;
use super::presign::presign_url;
use super::presign::tests::request;
use super::testing::{BUCKET, Reply, Rig, texts};
use super::{GcKey, PresignKey};

const MEBIBYTE: u64 = 1 << 20;
const GC: (&str, &str) = ("pack-gc", "gc-secret-0123456789");

async fn rig() -> Rig {
    let made = Rig::with(MEBIBYTE, |config| {
        config.gc_keys = vec![GcKey::new(GC.0, GC.1, "cas/b/").unwrap()];
        let grant = super::presign::tests::grant();
        config.presign_keys =
            vec![PresignKey::new("pack-presign", "presign-secret", grant).unwrap()];
    });
    made.await
}

fn uri(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

fn listing(query: &str) -> String {
    format!("/{BUCKET}?{query}")
}

async fn gc(rig: &Rig, method: &str, uri: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
    rig.send_as(GC, method, uri, extra, body).await
}

fn denied(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn access_denied() -> (u16, String) {
    (403, "AccessDenied".to_owned())
}

/// Asserts the calls a GC key does under its prefix: ListObjectsV2 with a prefix inside its own
/// (the prefix itself and a deeper one), HeadObject (the size), and DeleteObject, which removes the
/// object from both tiers (one object is in the hot tier, one in the cold tier only) and
/// journals the delete; the objects outside the prefix are untouched.
#[tokio::test(start_paused = true)]
async fn a_gc_key_lists_heads_and_deletes_under_its_prefix() {
    let rig = rig().await;
    rig.put("cas/b/x/1", b"one").await;
    rig.put("cas/b/x/2", b"two").await;
    rig.put("cas/c/1", b"other").await;
    rig.cold_mem
        .put("cas/b/cold", bytes::Bytes::from_static(b"cold"))
        .await
        .unwrap();
    let all = gc(
        &rig,
        "GET",
        &listing("list-type=2&prefix=cas%2Fb%2F"),
        &[],
        b"",
    )
    .await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(
        texts(&all.body, "Key"),
        ["cas/b/cold", "cas/b/x/1", "cas/b/x/2"]
    );
    let deeper = gc(
        &rig,
        "GET",
        &listing("list-type=2&prefix=cas%2Fb%2Fx%2F"),
        &[],
        b"",
    )
    .await;
    assert_eq!(texts(&deeper.body, "Key"), ["cas/b/x/1", "cas/b/x/2"]);
    let head = gc(&rig, "HEAD", &uri("cas/b/x/1"), &[], b"").await;
    assert_eq!(
        (head.status, head.header("content-length")),
        (StatusCode::OK, "3")
    );
    let journals = rig.hot_mem.list("_journal/p1/").await.unwrap().len();
    for key in ["cas/b/x/1", "cas/b/cold"] {
        let deleted = gc(&rig, "DELETE", &uri(key), &[], b"").await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{key}");
        assert!(rig.hot_mem.get(key, None).await.is_err(), "{key}");
        assert!(rig.cold_mem.get(key, None).await.is_err(), "{key}");
    }
    assert!(rig.hot_mem.list("_journal/p1/").await.unwrap().len() > journals);
    assert_eq!(rig.get("cas/b/x/2", None).await.body, "two");
    assert_eq!(rig.get("cas/c/1", None).await.body, "other");
}

/// Asserts that a listing with a prefix outside the key's is refused and not filtered: no prefix,
/// an empty one, a shorter one, the prefix without its slash, a sibling, every spelling of `..`
/// that would climb out, and the prefixes of the proxy; no key name is in any answer.
#[tokio::test(start_paused = true)]
async fn a_list_prefix_outside_the_gc_prefix_is_refused() {
    let rig = rig().await;
    rig.put("cas/b/x", b"1").await;
    rig.put("cas/c/secret-name", b"2").await;
    for query in [
        "list-type=2",
        "list-type=2&prefix=",
        "list-type=2&prefix=cas%2F",
        "list-type=2&prefix=cas%2Fb",
        "list-type=2&prefix=cas%2Fc%2F",
        "list-type=2&prefix=cas%2Fb%2F..%2Fc%2F",
        "list-type=2&prefix=cas%2Fb%2F%2e%2e%2Fc%2F",
        "list-type=2&prefix=cas%2Fb%2F%2E%2E",
        "list-type=2&prefix=cas%2Fb%2F.%2Fc",
        "list-type=2&prefix=cas%2Fb%2F%00",
        "list-type=2&prefix=_journal%2F",
        "list-type=2&prefix=_tiering%2F",
        "list-type=2&prefix=%2F",
    ] {
        let reply = gc(&rig, "GET", &listing(query), &[], b"").await;
        assert_eq!(denied(&reply), access_denied(), "{query}");
        assert!(
            !reply.body.contains("secret-name") && !reply.body.contains("cas/b/x"),
            "{query}"
        );
    }
}

/// Asserts that a GET of an object is refused, with a range or without, and no byte of the
/// object is in the answer: a GC key never reads a body.
#[tokio::test(start_paused = true)]
async fn a_gc_key_never_reads_a_body() {
    let rig = rig().await;
    rig.put("cas/b/x", b"CONTENT-OF-X").await;
    for extra in [
        &[][..],
        &[("range", "bytes=0-3")][..],
        &[("if-none-match", "\"t\"")][..],
    ] {
        let reply = gc(&rig, "GET", &uri("cas/b/x"), extra, b"").await;
        assert_eq!(denied(&reply), access_denied());
        assert!(!reply.body.contains("CONTENT-OF-X"));
    }
}

/// Asserts that every call that writes is refused and stores nothing: a put (with and without the
/// create-only header), a part, the start and the completion of an upload, a copy, and the calls
/// on the bucket itself (CreateBucket, HeadBucket).
#[tokio::test(start_paused = true)]
async fn a_gc_key_writes_nothing_and_does_not_touch_the_bucket() {
    let rig = rig().await;
    let create = [("if-none-match", "*")];
    type Call<'a> = (&'a str, String, &'a [(&'a str, &'a str)]);
    let calls: Vec<Call<'_>> = vec![
        ("PUT", uri("cas/b/new"), &[]),
        ("PUT", uri("cas/b/new"), &create),
        ("PUT", uri("cas/b/new?partNumber=1&uploadId=u"), &[]),
        ("POST", uri("cas/b/new?uploads"), &[]),
        ("POST", uri("cas/b/new?uploadId=u"), &[]),
        (
            "PUT",
            uri("cas/b/new"),
            &[("x-amz-copy-source", "/tier-bucket/cas/b/other")],
        ),
        ("PUT", format!("/{BUCKET}"), &[]),
        ("HEAD", format!("/{BUCKET}"), &[]),
        ("POST", uri("cas/c/new?uploads"), &[]),
    ];
    for (method, target, extra) in calls {
        let reply = gc(&rig, method, &target, extra, b"x").await;
        assert_eq!(denied(&reply), access_denied(), "{method} {target}");
    }
    assert!(rig.hot_mem.list("cas/").await.unwrap().is_empty());
    assert!(rig.cold_mem.list("").await.unwrap().is_empty());
    assert_eq!(
        rig.hot.calls(Op::CreateUpload) + rig.cold.calls(Op::CreateUpload),
        0
    );
}

/// Asserts that a head, a delete and an abort of a key outside the prefix are refused and change
/// nothing, in every spelling that would reach out of it, for the prefix itself and for the keys
/// of the proxy.
#[tokio::test(start_paused = true)]
async fn a_gc_key_touches_no_key_outside_its_prefix() {
    let rig = rig().await;
    rig.put("cas/c/keep", b"keep").await;
    for name in [
        "cas/c/keep",
        "cas/b/../c/keep",
        "cas/b/%2e%2e/c/keep",
        "cas/b/%2E%2E/c/keep",
        "cas/b/..%2Fc/keep",
        "cas/b/./x",
        "cas%2Fc%2Fkeep",
        "cas/b",
        "cas/b/",
        "cas/bb/x",
        "cas/b/x%00",
        "_journal/p1/0000000000000001",
        "_tiering/usage",
    ] {
        for method in ["HEAD", "DELETE"] {
            let reply = gc(&rig, method, &uri(name), &[], b"").await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {name}");
        }
        let abort = gc(
            &rig,
            "DELETE",
            &uri(&format!("{name}?uploadId=u")),
            &[],
            b"",
        )
        .await;
        assert_eq!(abort.status, StatusCode::FORBIDDEN, "abort {name}");
    }
    assert_eq!(rig.get("cas/c/keep", None).await.body, "keep");
}

/// Asserts that an upload begun under the prefix is aborted by the GC key (204), and that an abort
/// of an upload of a key outside the prefix is refused and leaves that upload in place, so that
/// a later completion by the owner still works.
#[tokio::test(start_paused = true)]
async fn a_gc_key_aborts_uploads_under_its_prefix_only() {
    let rig = rig().await;
    let begin = |key: &'static str| {
        let rig = &rig;
        async move {
            let started = rig.call("POST", &format!("{key}?uploads"), b"").await;
            texts(&started.body, "UploadId").concat()
        }
    };
    let mine = begin("cas/b/up").await;
    let theirs = begin("cas/c/up").await;
    let abort = |key: &str, id: &str| uri(&format!("{key}?uploadId={id}"));
    let refused = gc(&rig, "DELETE", &abort("cas/c/up", &theirs), &[], b"").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let part = rig
        .call(
            "PUT",
            &format!("cas/c/up?partNumber=1&uploadId={theirs}"),
            b"p",
        )
        .await;
    let etag = part.header("etag").to_owned();
    let done = completion(&[(1, etag.as_str())]);
    let end = rig
        .call(
            "POST",
            &format!("cas/c/up?uploadId={theirs}"),
            done.as_bytes(),
        )
        .await;
    assert_eq!(end.status, StatusCode::OK);
    let aborted = gc(&rig, "DELETE", &abort("cas/b/up", &mine), &[], b"").await;
    assert_eq!(aborted.status, StatusCode::NO_CONTENT);
    let again = rig
        .call(
            "PUT",
            &format!("cas/b/up?partNumber=1&uploadId={mine}"),
            b"p",
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

/// Asserts that a list of multipart uploads (`GET ?uploads`) passes the role of the key with a
/// prefix inside its own and is refused (403) with none or one outside: the front's own answer to
/// the call, whatever it is, is not a refusal of the role.
#[tokio::test(start_paused = true)]
async fn a_list_of_uploads_is_held_to_the_prefix_of_the_gc_key() {
    let rig = rig().await;
    let inside = gc(&rig, "GET", &listing("uploads&prefix=cas%2Fb%2F"), &[], b"").await;
    assert_ne!(inside.status, StatusCode::FORBIDDEN, "{}", inside.body);
    for query in [
        "uploads",
        "uploads&prefix=",
        "uploads&prefix=cas%2Fc%2F",
        "uploads&prefix=cas%2F",
    ] {
        let reply = gc(&rig, "GET", &listing(query), &[], b"").await;
        assert_eq!(denied(&reply), access_denied(), "{query}");
    }
}

/// Asserts that a presigned URL signed with a GC key is refused, for a PUT and for a GET: a URL's
/// key must be a presigning key, and a GC key signs in the header only; and that a GC key id
/// is not accepted for a presigning key's URL either way round (the presigning key in the header is
/// refused as before).
#[tokio::test(start_paused = true)]
async fn a_presigned_url_signed_with_a_gc_key_is_refused() {
    let rig = rig().await;
    rig.put("cas/b/x", b"CONTENT-OF-X").await;
    let key = AccessKey::new(GC.0, GC.1);
    for method in ["PUT", "GET", "DELETE", "HEAD"] {
        let url = presign_url(method, &key, &request("cas/b/x", 300), &[]).unwrap();
        let path = url.strip_prefix("http://tier.test").unwrap().to_owned();
        let signed = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "tier.test")
            .body(http_body_util::Full::new(bytes::Bytes::from_static(b"x")))
            .unwrap();
        let reply = rig.send_raw(signed).await;
        assert_eq!(
            denied(&reply),
            (403, "InvalidAccessKeyId".to_owned()),
            "{method}"
        );
        assert!(!reply.body.contains("CONTENT-OF-X"));
    }
    assert_eq!(rig.get("cas/b/x", None).await.body, "CONTENT-OF-X");
}
