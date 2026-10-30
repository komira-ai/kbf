//! What the server does with a request signed by a scoped key, through `Server::handle`.

use hyper::StatusCode;

use crate::Backend;

use super::AccessKey;
use super::auth::sign_as;
use super::ops_tests::completion;
use super::presign::presign_url;
use super::presign::tests::request;
use super::testing::{BUCKET, Reply, Rig, Scripted, Step, texts};
use super::{Prefixes, ScopedKey};

const MEBIBYTE: u64 = 1 << 20;

/// The key of the tests: read `r/` and `rw/`, create under `w/` and `rw/`, delete under `d/`.
const KEY: (&str, &str) = ("scoped-a", "scoped-secret-0123456789");

/// A key that reads `r/`, creates under `w/` and has no delete prefix.
const NO_DELETE: (&str, &str) = ("scoped-b", "other-secret-0123456789");

async fn rig() -> Rig {
    let made = Rig::with(MEBIBYTE, |config| {
        let a = Prefixes {
            read: &["r/", "rw/"],
            write: &["w/", "rw/"],
            delete: &["d/"],
        };
        let b = Prefixes {
            read: &["r/"],
            write: &["w/"],
            delete: &[],
        };
        let a = ScopedKey::new(KEY.0, KEY.1, &a);
        let b = ScopedKey::new(NO_DELETE.0, NO_DELETE.1, &b);
        config.scoped_keys = vec![a.unwrap(), b.unwrap()];
    });
    made.await
}

fn uri(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

fn listing(query: &str) -> String {
    format!("/{BUCKET}?{query}")
}

async fn scoped(rig: &Rig, method: &str, uri: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
    rig.send_as(KEY, method, uri, extra, body).await
}

fn denied(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn access_denied() -> (u16, String) {
    (403, "AccessDenied".to_owned())
}

/// Whether the full key finds the object `key`.
async fn exists(rig: &Rig, key: &str) -> bool {
    rig.get(key, None).await.status == StatusCode::OK
}

/// Asserts the reads of a scoped key inside its read prefixes: GetObject (whole and ranged),
/// HeadObject, ListObjectsV2 with a prefix that is a read prefix or deeper (also one that ends
/// inside a name), and ListMultipartUploads with such a prefix, which lists only the uploads
/// under it.
#[tokio::test(start_paused = true)]
async fn a_scoped_key_reads_and_lists_inside_its_read_prefixes() {
    let rig = rig().await;
    rig.put("r/a", b"one").await;
    rig.put("rw/b", b"two").await;
    rig.put("other/d", b"four").await;
    let got = scoped(&rig, "GET", &uri("r/a"), &[], b"").await;
    assert_eq!((got.status, got.body.as_str()), (StatusCode::OK, "one"));
    let ranged = scoped(&rig, "GET", &uri("rw/b"), &[("range", "bytes=0-1")], b"").await;
    assert_eq!(ranged.body, "tw");
    let head = scoped(&rig, "HEAD", &uri("r/a"), &[], b"").await;
    assert_eq!(
        (head.status, head.header("content-length")),
        (StatusCode::OK, "3")
    );
    for (query, keys) in [
        ("list-type=2&prefix=r%2F", vec!["r/a"]),
        ("list-type=2&prefix=rw%2F", vec!["rw/b"]),
        ("list-type=2&prefix=r%2Fa", vec!["r/a"]),
        ("list-type=2&prefix=rw%2Fz", vec![]),
    ] {
        let listed = scoped(&rig, "GET", &listing(query), &[], b"").await;
        assert_eq!(listed.status, StatusCode::OK, "{query}");
        assert_eq!(texts(&listed.body, "Key"), keys, "{query}");
    }
    let started = rig.call("POST", "r/up?uploads", b"").await;
    let id = texts(&started.body, "UploadId").concat();
    rig.call("POST", "other/up?uploads", b"").await;
    let open = scoped(&rig, "GET", &listing("uploads&prefix=r%2F"), &[], b"").await;
    assert_eq!(open.status, StatusCode::OK, "{}", open.body);
    assert_eq!(texts(&open.body, "Key"), ["r/up"]);
    assert_eq!(texts(&open.body, "UploadId"), [id.as_str()]);
}

/// Asserts that a read or a list outside the read prefixes is 403 and names no key: an object of
/// another prefix, of a write prefix and of a delete prefix (a prefix for another list is no
/// read prefix), the prefix itself as a key, a path with `..` that climbs out, the keys of the
/// proxy, and a listing with no prefix, an empty one, a shorter one, a sibling, one that climbs
/// out and the prefixes of the proxy, each for objects and for uploads.
#[tokio::test(start_paused = true)]
async fn a_read_or_a_list_outside_the_read_prefixes_is_refused() {
    let rig = rig().await;
    for key in ["w/c", "d/e", "other/d", "r/a"] {
        rig.put(key, b"CONTENT-OF-KEY").await;
    }
    rig.call("POST", "other/up?uploads", b"").await;
    for key in [
        "w/c",
        "d/e",
        "other/d",
        "r/",
        "r",
        "r/../other/d",
        "r/%2e%2e/other/d",
        "r/./a",
        "_journal/p1/x",
        "_tiering/x",
    ] {
        for method in ["GET", "HEAD"] {
            let reply = scoped(&rig, method, &uri(key), &[], b"").await;
            assert_eq!(denied(&reply), access_denied(), "{method} {key}");
            assert!(!reply.body.contains("CONTENT-OF-KEY"), "{method} {key}");
        }
    }
    for query in [
        "list-type=2",
        "list-type=2&prefix=",
        "list-type=2&prefix=w%2F",
        "list-type=2&prefix=d%2F",
        "list-type=2&prefix=other%2F",
        "list-type=2&prefix=r",
        "list-type=2&prefix=%2F",
        "list-type=2&prefix=r%2F..%2Fother%2F",
        "list-type=2&prefix=r%2F%00",
        "list-type=2&prefix=r%2F.%2F",
        "list-type=2&prefix=other%2F&prefix=r%2F",
        "list-type=2&prefix=_journal%2F",
        "list-type=2&prefix=_tiering%2F",
        "uploads",
        "uploads&prefix=",
        "uploads&prefix=other%2F",
        "uploads&prefix=w%2F",
    ] {
        let reply = scoped(&rig, "GET", &listing(query), &[], b"").await;
        assert_eq!(denied(&reply), access_denied(), "{query}");
        assert!(!reply.body.contains("other/"), "{query}");
    }
}

/// Asserts that the first `prefix` of a listing is the one that counts, for the check and for
/// the answer alike: a listing that names `r/` first and another prefix after lists `r/` only,
/// and `.` segments are refused for the delete and the multipart calls as for the others.
#[tokio::test(start_paused = true)]
async fn the_first_list_prefix_counts_and_dot_segments_are_refused_on_every_call() {
    let rig = rig().await;
    rig.put("r/a", b"one").await;
    rig.put("other/b", b"two").await;
    let query = listing("list-type=2&prefix=r%2F&prefix=other%2F");
    let first = scoped(&rig, "GET", &query, &[], b"").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(texts(&first.body, "Key"), ["r/a"]);
    for (method, key) in [
        ("DELETE", "d/./e"),
        ("DELETE", "d/../r/a"),
        ("POST", "w/./big?uploads"),
        ("PUT", "w/./x"),
    ] {
        let reply = scoped(&rig, method, &uri(key), &[("if-none-match", "*")], b"").await;
        assert_eq!(denied(&reply), access_denied(), "{method} {key}");
    }
    assert!(exists(&rig, "r/a").await);
}

/// Asserts that finishing a multipart upload never replaces an object: for a key under a write
/// prefix that already holds an object, the completion is not 200 and the object is as it was.
#[tokio::test(start_paused = true)]
async fn completing_an_upload_over_an_existing_key_does_not_replace_it() {
    let rig = rig().await;
    rig.put("w/there", b"old").await;
    let started = scoped(&rig, "POST", &uri("w/there?uploads"), &[], b"").await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let id = texts(&started.body, "UploadId").concat();
    let part = uri(&format!("w/there?partNumber=1&uploadId={id}"));
    let sent = scoped(&rig, "PUT", &part, &[], b"new").await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let done = completion(&[(1, sent.header("etag"))]);
    let complete = uri(&format!("w/there?uploadId={id}"));
    let end = scoped(&rig, "POST", &complete, &[], done.as_bytes()).await;
    assert_ne!(end.status, StatusCode::OK, "{}", end.body);
    assert_eq!(rig.get("w/there", None).await.body, "old");
}

/// Asserts that a write prefix is no read prefix: an object a scoped key created under `w/` is
/// answered by the full key and refused to the scoped key, for a get, a head and a list, unless
/// the prefix is also in the read list (`rw/`, which is in both).
#[tokio::test(start_paused = true)]
async fn a_write_prefix_without_a_read_prefix_cannot_be_read() {
    let rig = rig().await;
    for key in ["w/new", "rw/new"] {
        let made = scoped(&rig, "PUT", &uri(key), &[("if-none-match", "*")], b"made").await;
        assert_eq!(made.status, StatusCode::OK, "{key}");
        assert_eq!(rig.get(key, None).await.body, "made", "{key}");
    }
    let get = scoped(&rig, "GET", &uri("w/new"), &[], b"").await;
    assert_eq!(denied(&get), access_denied());
    let head = scoped(&rig, "HEAD", &uri("w/new"), &[], b"").await;
    assert_eq!(head.status, StatusCode::FORBIDDEN);
    let list = scoped(&rig, "GET", &listing("list-type=2&prefix=w%2F"), &[], b"").await;
    assert_eq!(denied(&list), access_denied());
    let overlap = scoped(&rig, "GET", &uri("rw/new"), &[], b"").await;
    assert_eq!(
        (overlap.status, overlap.body.as_str()),
        (StatusCode::OK, "made")
    );
}

/// Asserts the create-only put: inside a write prefix a put with `If-None-Match: *`, and one with
/// no condition, create the object; a put over an object that exists is 412 `PreconditionFailed`
/// and leaves it as it was; `If-Match` (a replace) and any other `If-None-Match` are 403, also
/// for a key that does not exist, and so is a copy; a put outside the write prefixes (a read
/// prefix, a delete prefix, another) is 403 and creates nothing.
#[tokio::test(start_paused = true)]
async fn a_scoped_put_creates_under_a_write_prefix_and_never_replaces() {
    let rig = rig().await;
    let star = [("if-none-match", "*")];
    let first = scoped(&rig, "PUT", &uri("w/x"), &star, b"first").await;
    assert_eq!(first.status, StatusCode::OK);
    let bare = scoped(&rig, "PUT", &uri("w/y/deep"), &[], b"bare").await;
    assert_eq!(bare.status, StatusCode::OK);
    assert_eq!(rig.get("w/x", None).await.body, "first");
    assert_eq!(rig.get("w/y/deep", None).await.body, "bare");
    for extra in [&star[..], &[][..]] {
        let again = scoped(&rig, "PUT", &uri("w/x"), extra, b"second").await;
        assert_eq!(
            (again.status.as_u16(), again.code()),
            (412, "PreconditionFailed".to_owned())
        );
    }
    for (key, extra) in [
        ("w/x", [("if-match", "\"abc\"")]),
        ("w/fresh", [("if-match", "\"abc\"")]),
        ("w/x", [("if-none-match", "\"abc\"")]),
        ("w/fresh", [("x-amz-copy-source", "/tier-bucket/w/x")]),
        ("w/x", [("x-amz-copy-source", "/tier-bucket/r/a")]),
    ] {
        let reply = scoped(&rig, "PUT", &uri(key), &extra, b"second").await;
        assert_eq!(denied(&reply), access_denied(), "{key} {extra:?}");
    }
    assert_eq!(rig.get("w/x", None).await.body, "first");
    assert!(!exists(&rig, "w/fresh").await);
    for key in [
        "r/x",
        "d/x",
        "other/x",
        "w/",
        "w",
        "w/../other/x",
        "_journal/p1/x",
    ] {
        let reply = scoped(&rig, "PUT", &uri(key), &star, b"nope").await;
        assert_eq!(denied(&reply), access_denied(), "{key}");
        assert!(!exists(&rig, key).await, "{key}");
    }
    assert!(rig.hot_mem.list("other/").await.unwrap().is_empty());
}

/// Asserts the calls of a multipart upload inside a write prefix: start, two parts, completion,
/// and the object read back whole; and an abort, after which a part is `NoSuchUpload`.
#[tokio::test(start_paused = true)]
async fn a_scoped_key_runs_a_multipart_upload_under_a_write_prefix() {
    let rig = rig().await;
    let started = scoped(&rig, "POST", &uri("w/big?uploads"), &[], b"").await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let id = texts(&started.body, "UploadId").concat();
    let mut tags = Vec::new();
    for (number, body) in [(1, b"hello ".as_slice()), (2, b"world".as_slice())] {
        let part = format!("w/big?partNumber={number}&uploadId={id}");
        let sent = scoped(&rig, "PUT", &uri(&part), &[], body).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        tags.push(sent.header("etag").to_owned());
    }
    let done = completion(&[(1, tags[0].as_str()), (2, tags[1].as_str())]);
    let end = scoped(
        &rig,
        "POST",
        &uri(&format!("w/big?uploadId={id}")),
        &[],
        done.as_bytes(),
    )
    .await;
    assert_eq!(end.status, StatusCode::OK, "{}", end.body);
    assert_eq!(rig.get("w/big", None).await.body, "hello world");
    let started = scoped(&rig, "POST", &uri("rw/gone?uploads"), &[], b"").await;
    let id = texts(&started.body, "UploadId").concat();
    let abort = uri(&format!("rw/gone?uploadId={id}"));
    let aborted = scoped(&rig, "DELETE", &abort, &[], b"").await;
    assert_eq!(aborted.status, StatusCode::NO_CONTENT);
    let part = uri(&format!("rw/gone?partNumber=1&uploadId={id}"));
    let late = scoped(&rig, "PUT", &part, &[], b"p").await;
    assert_eq!(late.status, StatusCode::NOT_FOUND);
}

/// Asserts that a multipart call outside the write prefixes is 403 at every step: the start for a
/// key under a read prefix, a delete prefix or another prefix; and, for an upload the full key
/// began at a key outside, the part, the completion and the abort (a delete prefix does not
/// allow an abort); each leaves the upload in place, so the full key completes it after, and a
/// part that copies is 403 inside the prefix.
#[tokio::test(start_paused = true)]
async fn a_multipart_call_outside_the_write_prefixes_is_refused_at_every_step() {
    let rig = rig().await;
    for key in ["r/big", "d/big", "other/big"] {
        let reply = scoped(&rig, "POST", &uri(&format!("{key}?uploads")), &[], b"").await;
        assert_eq!(denied(&reply), access_denied(), "start {key}");
    }
    for key in ["other/big", "d/big", "r/big"] {
        let started = rig.call("POST", &format!("{key}?uploads"), b"").await;
        let id = texts(&started.body, "UploadId").concat();
        let part = uri(&format!("{key}?partNumber=1&uploadId={id}"));
        let refused = scoped(&rig, "PUT", &part, &[], b"p").await;
        assert_eq!(denied(&refused), access_denied(), "part {key}");
        let sent = rig
            .call("PUT", &format!("{key}?partNumber=1&uploadId={id}"), b"p")
            .await;
        let tag = sent.header("etag").to_owned();
        let done = completion(&[(1, tag.as_str())]);
        let complete = uri(&format!("{key}?uploadId={id}"));
        let refused = scoped(&rig, "POST", &complete, &[], done.as_bytes()).await;
        assert_eq!(denied(&refused), access_denied(), "complete {key}");
        let refused = scoped(&rig, "DELETE", &complete, &[], b"").await;
        assert_eq!(denied(&refused), access_denied(), "abort {key}");
        assert!(!exists(&rig, key).await, "{key}");
        let ended = rig
            .call("POST", &format!("{key}?uploadId={id}"), done.as_bytes())
            .await;
        assert_eq!(ended.status, StatusCode::OK, "{key}: {}", ended.body);
        assert!(exists(&rig, key).await, "{key}");
    }
    let started = scoped(&rig, "POST", &uri("w/copy?uploads"), &[], b"").await;
    let id = texts(&started.body, "UploadId").concat();
    let part = uri(&format!("w/copy?partNumber=1&uploadId={id}"));
    let copy = [("x-amz-copy-source", "/tier-bucket/r/a")];
    let refused = scoped(&rig, "PUT", &part, &copy, b"").await;
    assert_eq!(denied(&refused), access_denied());
}

/// Asserts that an upload id belongs to the key it was created for: an upload of `w/a` is
/// `NoSuchUpload` for a part and for a completion naming `w/b`, both inside the write prefix, and
/// `w/b` is not created; the upload still completes under its own key.
#[tokio::test(start_paused = true)]
async fn an_upload_id_made_for_one_key_is_refused_for_another_inside_the_write_prefix() {
    let rig = rig().await;
    let started = scoped(&rig, "POST", &uri("w/a?uploads"), &[], b"").await;
    let id = texts(&started.body, "UploadId").concat();
    let own = uri(&format!("w/a?partNumber=1&uploadId={id}"));
    let sent = scoped(&rig, "PUT", &own, &[], b"p").await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
    let tag = sent.header("etag").to_owned();
    let other = uri(&format!("w/b?partNumber=1&uploadId={id}"));
    let part = scoped(&rig, "PUT", &other, &[], b"p").await;
    assert_eq!(
        (part.status.as_u16(), part.code()),
        (404, "NoSuchUpload".to_owned())
    );
    let done = completion(&[(1, tag.as_str())]);
    let complete = uri(&format!("w/b?uploadId={id}"));
    let ended = scoped(&rig, "POST", &complete, &[], done.as_bytes()).await;
    assert_eq!(
        (ended.status.as_u16(), ended.code()),
        (404, "NoSuchUpload".to_owned())
    );
    assert!(!exists(&rig, "w/b").await);
    let complete = uri(&format!("w/a?uploadId={id}"));
    let ended = scoped(&rig, "POST", &complete, &[], done.as_bytes()).await;
    assert_eq!(ended.status, StatusCode::OK, "{}", ended.body);
    assert!(exists(&rig, "w/a").await);
    assert!(!exists(&rig, "w/b").await);
}

/// Asserts that DeleteObject is allowed inside a delete prefix only: the object leaves both tiers
/// and the delete is journaled; a delete under a read prefix, a write prefix, another prefix or the
/// prefix itself is 403 and leaves the object; a key whose delete list is empty cannot delete
/// anywhere, and a key reads and deletes only under its own lists, not another key's.
#[tokio::test(start_paused = true)]
async fn a_scoped_key_deletes_inside_its_delete_prefixes_only() {
    let rig = rig().await;
    for key in ["d/a", "d/b", "r/a", "w/c", "rw/e", "other/f"] {
        rig.put(key, b"keep").await;
    }
    rig.cold_mem
        .put("d/cold", bytes::Bytes::from_static(b"cold"))
        .await
        .unwrap();
    let journals = rig.hot_mem.list("_journal/p1/").await.unwrap().len();
    for key in ["d/a", "d/cold"] {
        let deleted = scoped(&rig, "DELETE", &uri(key), &[], b"").await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{key}");
        assert!(rig.hot_mem.get(key, None).await.is_err(), "{key}");
        assert!(rig.cold_mem.get(key, None).await.is_err(), "{key}");
    }
    assert!(rig.hot_mem.list("_journal/p1/").await.unwrap().len() > journals);
    for key in ["r/a", "w/c", "rw/e", "other/f", "d/", "d", "_journal/p1/x"] {
        let reply = scoped(&rig, "DELETE", &uri(key), &[], b"").await;
        assert_eq!(denied(&reply), access_denied(), "{key}");
    }
    for key in ["d/b", "r/a", "w/c", "rw/e", "other/f"] {
        assert!(exists(&rig, key).await, "{key}");
    }
    let other = |method: &'static str, key: &'static str| {
        let rig = &rig;
        async move { rig.send_as(NO_DELETE, method, &uri(key), &[], b"").await }
    };
    assert_eq!(denied(&other("DELETE", "d/b").await), access_denied());
    assert_eq!(denied(&other("DELETE", "w/c").await), access_denied());
    assert_eq!(denied(&other("GET", "rw/e").await), access_denied());
    assert_eq!(other("GET", "r/a").await.status, StatusCode::OK);
    assert!(exists(&rig, "d/b").await && exists(&rig, "w/c").await);
}

/// Asserts that a call a scoped key cannot do is 403 before its body is looked at: a put, a get, a
/// delete and an upload start outside the prefixes, and a replacing put, each with a body that
/// fails when read; that a put the key may do is held to `--max-request-bytes` (413) as for any
/// key; and that the other methods and the calls on the bucket and the root (a put, a head, a
/// delete, a post, a get with no prefix) are 403 as well. Nothing is stored.
#[tokio::test(start_paused = true)]
async fn a_refused_scoped_call_is_403_without_its_body_being_read() {
    let rig = Rig::with(MEBIBYTE, |config| {
        let key = ScopedKey::new(
            KEY.0,
            KEY.1,
            &Prefixes {
                read: &["r/"],
                write: &["w/"],
                delete: &["d/"],
            },
        )
        .unwrap();
        config.scoped_keys = vec![key];
        config.max_request_bytes = 4;
    })
    .await;
    let signed = |method: &str, path: &str, extra: &[(&str, &str)]| {
        sign_as(&rig.config, KEY, method, path, extra, b"x")
    };
    for (method, path, extra) in [
        ("PUT", uri("other/x"), &[][..]),
        ("GET", uri("other/x"), &[][..]),
        ("DELETE", uri("other/x"), &[][..]),
        ("POST", uri("other/x?uploads"), &[][..]),
        ("PUT", uri("w/x"), &[("if-match", "\"a\"")][..]),
    ] {
        let (parts, _) = signed(method, &path, extra).into_parts();
        let failing = Scripted::new(vec![Step::Fail]);
        let reply = rig
            .send_raw(hyper::Request::from_parts(parts, failing))
            .await;
        assert_eq!(denied(&reply), access_denied(), "{method} {path} {extra:?}");
    }
    // Allowed by the key, so the limit answers: the declared length is over it.
    let (parts, _) = signed("PUT", &uri("w/x"), &[("content-length", "10")]).into_parts();
    let failing = Scripted::new(vec![Step::Fail]);
    let reply = rig
        .send_raw(hyper::Request::from_parts(parts, failing))
        .await;
    assert_eq!(denied(&reply), (413, "EntityTooLarge".to_owned()));
    for (method, path) in [
        ("PATCH", uri("w/x")),
        ("OPTIONS", uri("w/x")),
        ("PUT", format!("/{BUCKET}")),
        ("HEAD", format!("/{BUCKET}")),
        ("DELETE", format!("/{BUCKET}")),
        ("POST", format!("/{BUCKET}?delete")),
        ("GET", format!("/{BUCKET}")),
        ("GET", "/".to_owned()),
        ("GET", uri("r/a?uploadId=1")),
        ("POST", uri("w/x")),
    ] {
        let reply = scoped(&rig, method, &path, &[], b"x").await;
        assert_eq!(denied(&reply), access_denied(), "{method} {path}");
    }
    for key in ["w/x", "other/x"] {
        assert!(!exists(&rig, key).await, "{key}");
    }
}

/// Asserts that a presigned URL signed with a scoped key is refused, for every method: a URL's
/// key must be a presigning key, and a scoped key signs in the header only; the objects stay.
#[tokio::test(start_paused = true)]
async fn a_presigned_url_signed_with_a_scoped_key_is_refused() {
    let rig = rig().await;
    rig.put("r/a", b"CONTENT-OF-A").await;
    let key = AccessKey::new(KEY.0, KEY.1);
    for method in ["PUT", "GET", "DELETE", "HEAD"] {
        let url = presign_url(method, &key, &request("w/a", 300), &[]).unwrap();
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
    }
    assert!(!exists(&rig, "w/a").await);
    assert_eq!(rig.get("r/a", None).await.body, "CONTENT-OF-A");
}
