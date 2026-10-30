//! The routes of the front, over TLS.

use crate::client::Reply;
use crate::rig::{Rig, hex_of};

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;

/// A blob of `len` bytes that does not repeat within a chunk.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
}

#[track_caller]
fn assert_error(reply: &Reply, status: u16, code: &str) {
    assert_eq!(reply.status, status, "{reply:?}");
    assert_eq!(reply.error_code(), code);
    assert_eq!(
        reply.must("content-type"),
        "application/json; charset=utf-8"
    );
    assert_eq!(
        reply.must("docker-distribution-api-version"),
        "registry/2.0"
    );
}

/// Asserts the version check over TLS: `GET` and `HEAD` of `/v2/` are 200 with `{}` (a `HEAD`
/// with its length and no body) and the registry version header, with or without an
/// `Authorization` header, which is not challenged.
#[tokio::test]
async fn the_ping_answers_over_tls() {
    let rig = Rig::start().await;
    let got = rig.get("/v2/").await;
    assert_eq!(got.status, 200);
    assert_eq!(got.body, b"{}");
    assert_eq!(got.must("docker-distribution-api-version"), "registry/2.0");
    assert_eq!(got.must("content-length"), "2");
    let head = rig.ask("HEAD", "/v2/", &[]).await.expect("an answer");
    assert_eq!((head.status, head.body.len(), head.short), (200, 0, false));
    assert_eq!(head.must("content-length"), "2");
    let token = [("authorization", "Bearer something")];
    let with_token = rig.ask("GET", "/v2/", &token).await.expect("an answer");
    assert_eq!(with_token.status, 200);
    assert!(with_token.header("www-authenticate").is_none());
    rig.stop().await;
}

/// Asserts a manifest is served over TLS with exactly the headers a client reads: the media
/// type, the length, the digest as `Docker-Content-Digest` and as a quoted `ETag`; a `HEAD`
/// gives the same without a body.
#[tokio::test]
async fn a_manifest_is_served_with_exact_headers() {
    let rig = Rig::start().await;
    let digest = rig.seed(MANIFEST.as_bytes()).await;
    let path = format!("/v2/library/app/manifests/{digest}");
    let got = rig.get(&path).await;
    let head = rig.ask("HEAD", &path, &[]).await.expect("an answer");
    for reply in [&got, &head] {
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.must("content-type"),
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(reply.must("content-length"), MANIFEST.len().to_string());
        assert_eq!(reply.must("docker-content-digest"), digest);
        assert_eq!(reply.must("etag"), format!("\"{digest}\""));
        assert_eq!(
            reply.must("docker-distribution-api-version"),
            "registry/2.0"
        );
    }
    assert_eq!(got.body, MANIFEST.as_bytes());
    assert!(head.body.is_empty() && !head.short);
    rig.stop().await;
}

/// Asserts a blob of more than one chunk is streamed whole and exact, that a `HEAD` gives its
/// headers and no body, and that the bytes served are counted.
#[tokio::test]
async fn a_blob_is_streamed_in_full() {
    let rig = Rig::start().await;
    let content = pattern((4 << 20) + (1 << 20) + 123);
    let digest = rig.seed(&content).await;
    let path = format!("/v2/library/app/blobs/{digest}");
    let got = rig.get(&path).await;
    assert_eq!(got.status, 200);
    assert_eq!(got.must("content-type"), "application/octet-stream");
    assert_eq!(got.must("content-length"), content.len().to_string());
    assert_eq!(got.must("docker-content-digest"), digest);
    assert_eq!(got.must("etag"), format!("\"{digest}\""));
    assert_eq!(got.must("accept-ranges"), "bytes");
    assert!(!got.short);
    assert!(got.body == content, "the body is the blob");
    let head = rig.ask("HEAD", &path, &[]).await.expect("an answer");
    assert_eq!(head.status, 200);
    assert_eq!(head.must("content-length"), content.len().to_string());
    assert!(head.body.is_empty() && !head.short);
    assert_eq!(rig.metrics.bytes_sent(), content.len() as u64);
    rig.stop().await;
}

/// Asserts a single range is 206 with the bytes and a `Content-Range` (one that crosses the
/// 4 MiB chunk of a whole read, a suffix, an open end), and a range past the end is 416 with
/// `Content-Range: bytes */size`.
#[tokio::test]
async fn a_range_is_206_and_a_range_past_the_end_is_416() {
    let rig = Rig::start().await;
    let content = pattern((4 << 20) + 1000);
    let digest = rig.seed(&content).await;
    let path = format!("/v2/a/blobs/{digest}");
    let size = content.len();
    let edge = 4 << 20;
    let cases = [
        (
            format!("bytes={}-{}", edge - 5, edge + 5),
            edge - 5,
            edge + 6,
        ),
        ("bytes=0-0".to_owned(), 0, 1),
        (format!("bytes={}-", size - 10), size - 10, size),
        ("bytes=-100".to_owned(), size - 100, size),
    ];
    for (range, from, to) in cases {
        let reply = rig
            .ask("GET", &path, &[("range", &range)])
            .await
            .expect("an answer");
        assert_eq!(reply.status, 206, "{range}");
        assert!(reply.body == content[from..to], "{range}");
        let expected = format!("bytes {from}-{}/{size}", to - 1);
        assert_eq!(reply.must("content-range"), expected);
        assert_eq!(reply.must("content-length"), (to - from).to_string());
    }
    let past = format!("bytes={size}-");
    let reply = rig
        .ask("GET", &path, &[("range", &past)])
        .await
        .expect("an answer");
    assert_error(&reply, 416, "RANGE_NOT_SATISFIABLE");
    assert_eq!(reply.must("content-range"), format!("bytes */{size}"));
    rig.stop().await;
}

/// Asserts a tag in place of a digest is 404 `MANIFEST_UNKNOWN` that says tags are not served.
#[tokio::test]
async fn a_tag_is_refused() {
    let rig = Rig::start().await;
    let reply = rig.get("/v2/library/app/manifests/latest").await;
    assert_error(&reply, 404, "MANIFEST_UNKNOWN");
    assert!(String::from_utf8_lossy(&reply.body).contains("tags are not served"));
    rig.stop().await;
}

/// Asserts that PUT, POST, PATCH and DELETE are 405 `UNSUPPORTED` with `Allow: GET, HEAD`,
/// on a blob, a manifest and the upload path of a push, and that nothing is written.
#[tokio::test]
async fn writes_are_405() {
    let rig = Rig::start().await;
    let digest = rig.seed(b"layer").await;
    let paths = [
        format!("/v2/a/blobs/{digest}"),
        format!("/v2/a/manifests/{digest}"),
        "/v2/a/blobs/uploads/".to_owned(),
    ];
    for method in ["PUT", "POST", "PATCH", "DELETE"] {
        for path in &paths {
            let reply = rig
                .ask(method, path, &[("content-length", "0")])
                .await
                .expect("an answer");
            assert_error(&reply, 405, "UNSUPPORTED");
            assert_eq!(reply.must("allow"), "GET, HEAD", "{method} {path}");
        }
    }
    let kept = rig.get(&format!("/v2/a/blobs/{digest}")).await;
    assert_eq!(kept.body, b"layer");
    rig.stop().await;
}

/// Asserts a digest that is not in the store is 404 `BLOB_UNKNOWN`, a malformed digest or name
/// is 400 (`DIGEST_INVALID`, `NAME_INVALID`), and a path that is no route is 404 `UNSUPPORTED`.
#[tokio::test]
async fn missing_and_malformed_requests_are_told_apart() {
    let rig = Rig::start().await;
    let absent = format!("sha256:{}", hex_of(b"absent"));
    assert_error(
        &rig.get(&format!("/v2/a/blobs/{absent}")).await,
        404,
        "BLOB_UNKNOWN",
    );
    assert_error(
        &rig.get(&format!("/v2/a/manifests/{absent}")).await,
        404,
        "MANIFEST_UNKNOWN",
    );
    assert_error(
        &rig.get("/v2/a/blobs/sha256:ABC").await,
        400,
        "DIGEST_INVALID",
    );
    assert_error(
        &rig.get("/v2/a/blobs/../../_journal/x").await,
        404,
        "UNSUPPORTED",
    );
    assert_error(
        &rig.get(&format!("/v2/A/blobs/{absent}")).await,
        400,
        "NAME_INVALID",
    );
    assert_error(&rig.get("/v2/a/tags/list").await, 404, "UNSUPPORTED");
    assert_error(&rig.get("/healthz").await, 404, "UNSUPPORTED");
    rig.stop().await;
}

/// Asserts a manifest whose bytes are not its digest is 500 and none of its bytes are sent.
#[tokio::test]
async fn a_manifest_that_is_not_its_digest_is_never_served() {
    let rig = Rig::start().await;
    let hex = hex_of(b"what the digest names");
    rig.put_under(&hex, MANIFEST.as_bytes()).await;
    let reply = rig.get(&format!("/v2/a/manifests/sha256:{hex}")).await;
    assert_error(&reply, 500, "DIGEST_INVALID");
    assert!(!String::from_utf8_lossy(&reply.body).contains("schemaVersion"));
    assert!(reply.header("docker-content-digest").is_none());
    assert_eq!(rig.metrics.mismatches(), 1);
    rig.stop().await;
}

/// Asserts a blob whose bytes are not its digest never reaches a client whole: the response is
/// cut short (or not begun), and the mismatch is counted.
#[tokio::test]
async fn a_blob_that_is_not_its_digest_ends_short() {
    let rig = Rig::start().await;
    let hex = hex_of(b"what the digest names");
    let wrong = pattern((4 << 20) + (1 << 20));
    rig.put_under(&hex, &wrong).await;
    let path = format!("/v2/a/blobs/sha256:{hex}");
    match rig.ask("GET", &path, &[]).await {
        None => {}
        Some(reply) => {
            assert_eq!(reply.must("content-length"), wrong.len().to_string());
            assert!(reply.short, "{} of {} bytes", reply.body.len(), wrong.len());
        }
    }
    assert_eq!(rig.metrics.mismatches(), 1);
    // The same object is still there for a range, which is a part of an object and unchecked.
    let part = rig
        .ask("GET", &path, &[("range", "bytes=0-9")])
        .await
        .expect("an answer");
    assert_eq!(part.status, 206);
    assert!(part.body == wrong[..10]);
    assert_eq!(rig.metrics.mismatches(), 1);
    rig.stop().await;
}

/// Asserts that an object only the cold tier has is served (read back through the proxy), and
/// that one connection carries several requests.
#[tokio::test]
async fn a_cold_object_is_served_and_a_connection_is_reused() {
    use bytes::Bytes;
    use tierstore::Backend as _;
    let rig = Rig::start().await;
    let content = pattern(300_000);
    let hex = hex_of(&content);
    let key = format!("oci/blobs/sha256/{hex}");
    rig.cold
        .put(&key, Bytes::from(content.clone()))
        .await
        .expect("put");
    let mut conn = rig.conn().await;
    let path = format!("/v2/a/blobs/sha256:{hex}");
    for _ in 0..3 {
        let reply = conn.request("GET", &path, &[]).await.expect("an answer");
        assert!(reply.status == 200 && reply.body == content);
    }
    let reply = conn.request("GET", "/v2/", &[]).await.expect("an answer");
    assert_eq!(reply.body, b"{}");
    assert_eq!(rig.metrics.requests("blob", "ok"), 3);
    rig.stop().await;
}
