//! The OCI front's answers, request by request, over an in-memory store: no socket.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt as _;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, StatusCode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{MAX_MANIFEST_BYTES, OciFront, OciMetrics, OciPrefix};
use crate::{Backend, Error, FaultyBackend, MemoryBackend, Op};

const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#;

fn hex_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct Rig {
    faulty: Arc<FaultyBackend>,
    memory: Arc<MemoryBackend>,
    front: OciFront,
    metrics: Arc<OciMetrics>,
}

impl Rig {
    fn new() -> Self {
        let memory = Arc::new(MemoryBackend::new());
        let faulty = Arc::new(FaultyBackend::new(memory.clone()));
        let metrics = Arc::new(OciMetrics::default());
        let prefix = OciPrefix::new("oci/").expect("a prefix");
        let front = OciFront::new(faulty.clone(), prefix, Arc::clone(&metrics));
        Self {
            faulty,
            memory,
            front,
            metrics,
        }
    }

    /// Stores `content` where its digest names it, and gives `sha256:<hex>`.
    async fn seed(&self, content: &[u8]) -> String {
        let hex = hex_of(content);
        self.put_under(&hex, content).await;
        format!("sha256:{hex}")
    }

    /// Stores `content` where `hex` names it, whatever it hashes to.
    async fn put_under(&self, hex: &str, content: &[u8]) {
        let key = format!("oci/blobs/sha256/{hex}");
        let put = self.memory.put(&key, Bytes::copy_from_slice(content)).await;
        put.expect("put");
    }

    async fn ask(&self, method: Method, path: &str, headers: &[(&str, &str)]) -> Answer {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).expect("a name");
            map.insert(name, HeaderValue::from_str(value).expect("a value"));
        }
        let response = self.front.handle(&method, path, &map).await;
        let (parts, mut body) = response.into_parts();
        let mut bytes = Vec::new();
        let mut cut = false;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => bytes.extend(frame.into_data().expect("data")),
                Err(_) => cut = true,
            }
        }
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: bytes,
            cut,
        }
    }

    async fn get(&self, path: &str) -> Answer {
        self.ask(Method::GET, path, &[]).await
    }
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
    /// The body ended in an error rather than at its end.
    cut: bool,
}

impl Answer {
    fn header(&self, name: &str) -> &str {
        let value = self
            .headers
            .get(name)
            .unwrap_or_else(|| panic!("no {name}"));
        value.to_str().expect("text")
    }

    fn has(&self, name: &str) -> bool {
        self.headers.contains_key(name)
    }

    /// The `(code, message, detail)` of the one error in the body.
    fn error(&self) -> (String, String, Value) {
        let document: Value = serde_json::from_slice(&self.body).expect("an error document");
        let errors = document["errors"].as_array().expect("errors");
        assert_eq!(errors.len(), 1, "{document}");
        let code = errors[0]["code"].as_str().expect("code").to_owned();
        let message = errors[0]["message"].as_str().expect("message").to_owned();
        (code, message, errors[0]["detail"].clone())
    }

    #[track_caller]
    fn assert_error(&self, status: StatusCode, code: &str) -> (String, Value) {
        assert_eq!(
            self.status,
            status,
            "{}",
            String::from_utf8_lossy(&self.body)
        );
        assert_eq!(
            self.header("content-type"),
            "application/json; charset=utf-8"
        );
        assert_eq!(self.header("content-length"), self.body.len().to_string());
        assert_eq!(
            self.header("docker-distribution-api-version"),
            "registry/2.0"
        );
        let (found, message, detail) = self.error();
        assert_eq!(found, code, "{message}");
        (message, detail)
    }
}

/// Asserts the version check: 200, `{}` and the registry version header, for `GET /v2/`, with or
/// without an `Authorization` header, which is never challenged.
#[tokio::test]
async fn the_ping_is_200_and_an_empty_object() {
    let rig = Rig::new();
    let plain = rig.get("/v2/").await;
    let token = ("authorization", "Bearer anything");
    let with_token = rig.ask(Method::GET, "/v2/", &[token]).await;
    for answer in [plain, with_token] {
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body, b"{}");
        assert_eq!(
            answer.header("docker-distribution-api-version"),
            "registry/2.0"
        );
        assert_eq!(
            answer.header("content-type"),
            "application/json; charset=utf-8"
        );
        assert_eq!(answer.header("content-length"), "2");
        assert!(!answer.has("www-authenticate"));
    }
    assert_eq!(rig.metrics.requests("ping", "ok"), 2);
}

/// Asserts a manifest is served with the headers a client needs, exactly: the media type it
/// declares, its length, its digest twice (header and quoted entity tag), and that `HEAD` gives
/// the same headers and no body.
#[tokio::test]
async fn a_manifest_is_served_with_its_type_length_and_digest() {
    let rig = Rig::new();
    let digest = rig.seed(MANIFEST.as_bytes()).await;
    let path = format!("/v2/library/app/manifests/{digest}");
    let got = rig.get(&path).await;
    let head = rig.ask(Method::HEAD, &path, &[]).await;
    for answer in [&got, &head] {
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(
            answer.header("content-type"),
            "application/vnd.oci.image.manifest.v1+json"
        );
        assert_eq!(answer.header("content-length"), MANIFEST.len().to_string());
        assert_eq!(answer.header("docker-content-digest"), digest);
        assert_eq!(answer.header("etag"), format!("\"{digest}\""));
        assert_eq!(
            answer.header("docker-distribution-api-version"),
            "registry/2.0"
        );
        assert!(!answer.has("accept-ranges"));
    }
    assert_eq!(got.body, MANIFEST.as_bytes());
    assert!(head.body.is_empty());
    assert_eq!(rig.metrics.requests("manifest", "ok"), 2);
    assert_eq!(rig.metrics.bytes_sent(), 0);
}

/// Asserts the four media types a manifest may declare come out as the `Content-Type`, and a
/// manifest that declares none is typed by its shape.
#[tokio::test]
async fn the_content_type_comes_from_the_manifest() {
    let rig = Rig::new();
    let docker = "application/vnd.docker.distribution.manifest.v2+json";
    let list = "application/vnd.docker.distribution.manifest.list.v2+json";
    let index = "application/vnd.oci.image.index.v1+json";
    let cases = [
        (
            format!(r#"{{"schemaVersion":2,"mediaType":"{docker}"}}"#),
            docker,
        ),
        (
            format!(r#"{{"schemaVersion":2,"mediaType":"{list}"}}"#),
            list,
        ),
        (
            format!(r#"{{"schemaVersion":2,"mediaType":"{index}","manifests":[]}}"#),
            index,
        ),
        (
            r#"{"schemaVersion":2,"config":{},"layers":[]}"#.to_owned(),
            "application/vnd.oci.image.manifest.v1+json",
        ),
        (r#"{"schemaVersion":2,"manifests":[]}"#.to_owned(), index),
    ];
    for (body, expected) in cases {
        let digest = rig.seed(body.as_bytes()).await;
        let answer = rig.get(&format!("/v2/a/manifests/{digest}")).await;
        assert_eq!(answer.status, StatusCode::OK, "{body}");
        assert_eq!(answer.header("content-type"), expected, "{body}");
    }
}

/// Asserts a stored object that is not a manifest is 500 `MANIFEST_INVALID` with the reason, and
/// its bytes are not sent.
#[tokio::test]
async fn an_object_that_is_not_a_manifest_is_refused() {
    let rig = Rig::new();
    for content in [
        &b"not json at all"[..],
        br#"{"schemaVersion":2,"mediaType":"x"}"#,
    ] {
        let digest = rig.seed(content).await;
        let answer = rig.get(&format!("/v2/a/manifests/{digest}")).await;
        let (message, detail) =
            answer.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "MANIFEST_INVALID");
        assert!(
            message.contains("not JSON") || message.contains("not a manifest"),
            "{message}"
        );
        assert_eq!(detail["digest"], digest);
        assert!(!String::from_utf8_lossy(&answer.body).contains("not json at all"));
    }
    assert_eq!(rig.metrics.requests("manifest", "failed"), 2);
}

/// Asserts a tag is 404 `MANIFEST_UNKNOWN` that says tags are not served, for GET and HEAD, and
/// that the store is not asked.
#[tokio::test]
async fn a_tag_is_refused_because_only_digests_are_served() {
    let rig = Rig::new();
    for method in [Method::GET, Method::HEAD] {
        let answer = rig.ask(method, "/v2/a/b/manifests/latest", &[]).await;
        let (message, detail) = answer.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
        assert!(message.contains("tags are not served"), "{message}");
        assert!(message.contains("by digest"), "{message}");
        assert_eq!(detail["tag"], "latest");
    }
    assert_eq!(rig.faulty.calls(Op::Get) + rig.faulty.calls(Op::Head), 0);
    assert_eq!(rig.metrics.requests("manifest", "not_found"), 2);
}

/// Asserts a reference that is neither a digest nor a tag, and a digest that is not
/// `sha256:` and 64 lowercase hex digits, is 400 `DIGEST_INVALID`, for a manifest and a blob; a
/// bad repository name is 400 `NAME_INVALID`.
#[tokio::test]
async fn malformed_digests_and_names_are_400() {
    let rig = Rig::new();
    let hex = hex_of(b"x");
    let bad_digests = [
        format!("sha512:{hex}"),
        format!("sha256:{}", hex.to_uppercase()),
        format!("sha256:{}", &hex[1..]),
        "sha256:".to_owned(),
        "not:a:digest".to_owned(),
    ];
    for digest in bad_digests {
        for kind in ["manifests", "blobs"] {
            let answer = rig.get(&format!("/v2/a/{kind}/{digest}")).await;
            let (_, detail) = answer.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
            assert_eq!(detail["digest"], digest);
        }
    }
    let good = format!("sha256:{hex}");
    for name in ["A", "a//b", "-a", ""] {
        for kind in ["manifests", "blobs"] {
            let answer = rig.get(&format!("/v2/{name}/{kind}/{good}")).await;
            let (_, detail) = answer.assert_error(StatusCode::BAD_REQUEST, "NAME_INVALID");
            assert_eq!(detail["name"], name);
        }
    }
    assert_eq!(rig.metrics.requests("manifest", "invalid"), 9);
    assert_eq!(rig.metrics.requests("blob", "invalid"), 9);
}

/// Asserts that a manifest or blob that is not in the store is 404 with the code of its kind.
#[tokio::test]
async fn a_missing_object_is_404_with_the_code_of_its_kind() {
    let rig = Rig::new();
    let digest = format!("sha256:{}", hex_of(b"absent"));
    let manifest = rig.get(&format!("/v2/a/manifests/{digest}")).await;
    let (_, detail) = manifest.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
    assert_eq!(detail["digest"], digest);
    for method in [Method::GET, Method::HEAD] {
        let blob = rig.ask(method, &format!("/v2/a/blobs/{digest}"), &[]).await;
        let (_, detail) = blob.assert_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN");
        assert_eq!(detail["digest"], digest);
    }
    assert_eq!(rig.metrics.requests("blob", "not_found"), 2);
}

/// Asserts that only the key under the prefix, named by the digest, is read: an object with the
/// same digest outside the prefix is not served.
#[tokio::test]
async fn only_the_prefixed_key_is_read() {
    let rig = Rig::new();
    let hex = hex_of(b"elsewhere");
    for key in [
        format!("blobs/sha256/{hex}"),
        format!("other/blobs/sha256/{hex}"),
    ] {
        rig.memory
            .put(&key, Bytes::from_static(b"elsewhere"))
            .await
            .expect("put");
    }
    let answer = rig.get(&format!("/v2/a/blobs/sha256:{hex}")).await;
    answer.assert_error(StatusCode::NOT_FOUND, "BLOB_UNKNOWN");
}

/// Asserts that a manifest whose bytes are not its digest is 500 `DIGEST_INVALID`, is counted,
/// and none of its bytes are sent.
#[tokio::test]
async fn a_manifest_that_is_not_its_digest_is_never_served() {
    let rig = Rig::new();
    let hex = hex_of(b"the digest names these bytes");
    rig.put_under(&hex, MANIFEST.as_bytes()).await;
    let answer = rig.get(&format!("/v2/a/manifests/sha256:{hex}")).await;
    let (message, _) = answer.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "DIGEST_INVALID");
    assert!(message.contains("does not match"), "{message}");
    assert!(!String::from_utf8_lossy(&answer.body).contains("schemaVersion"));
    assert!(!answer.has("docker-content-digest"));
    assert_eq!(rig.metrics.mismatches(), 1);
    assert_eq!(rig.metrics.requests("manifest", "failed"), 1);
}

/// Asserts the manifest size limit: a manifest of exactly the limit is served, one byte more is
/// 404 `MANIFEST_UNKNOWN` that names the limit.
#[tokio::test]
async fn a_manifest_over_the_limit_is_not_served() {
    let rig = Rig::new();
    let head = r#"{"schemaVersion":2,"config":{},"layers":[],"pad":""#;
    let fill = |extra: usize| {
        let pad = "a".repeat(MAX_MANIFEST_BYTES as usize + extra - head.len() - 2);
        format!("{head}{pad}\"}}")
    };
    let exact = fill(0);
    assert_eq!(exact.len() as u64, MAX_MANIFEST_BYTES);
    let digest = rig.seed(exact.as_bytes()).await;
    let served = rig.get(&format!("/v2/a/manifests/{digest}")).await;
    assert_eq!(served.status, StatusCode::OK);
    assert_eq!(served.body.len() as u64, MAX_MANIFEST_BYTES);
    let over = fill(1);
    let digest = rig.seed(over.as_bytes()).await;
    let refused = rig.get(&format!("/v2/a/manifests/{digest}")).await;
    let (message, _) = refused.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
    assert!(
        message.contains(&MAX_MANIFEST_BYTES.to_string()),
        "{message}"
    );
}

/// Asserts a blob is served with its length, its digest, `Accept-Ranges` and the octet-stream
/// type, that `HEAD` gives the same headers and no body, and that the bytes are counted.
#[tokio::test]
async fn a_blob_is_streamed_with_its_headers() {
    let rig = Rig::new();
    let content = b"layer bytes".repeat(1000);
    let digest = rig.seed(&content).await;
    let path = format!("/v2/a/blobs/{digest}");
    let got = rig.get(&path).await;
    let head = rig.ask(Method::HEAD, &path, &[]).await;
    for answer in [&got, &head] {
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.header("content-type"), "application/octet-stream");
        assert_eq!(answer.header("content-length"), content.len().to_string());
        assert_eq!(answer.header("docker-content-digest"), digest);
        assert_eq!(answer.header("etag"), format!("\"{digest}\""));
        assert_eq!(answer.header("accept-ranges"), "bytes");
        assert!(!answer.has("content-range"));
    }
    assert_eq!(got.body, content);
    assert!(!got.cut);
    assert!(head.body.is_empty());
    assert_eq!(rig.metrics.bytes_sent(), content.len() as u64);
    assert_eq!(rig.metrics.requests("blob", "ok"), 2);
}

/// Asserts a single range is 206 with the bytes and a `Content-Range`, for `a-b`, `a-` and
/// `-n`, for `HEAD` too (headers only), and that a range that is not understood is the whole blob.
#[tokio::test]
async fn a_range_is_206_with_a_content_range() {
    let rig = Rig::new();
    let digest = rig.seed(b"0123456789").await;
    let path = format!("/v2/a/blobs/{digest}");
    let cases = [
        ("bytes=2-4", "234", "bytes 2-4/10"),
        ("bytes=7-", "789", "bytes 7-9/10"),
        ("bytes=-2", "89", "bytes 8-9/10"),
        ("bytes=8-99", "89", "bytes 8-9/10"),
    ];
    for (range, expected, content_range) in cases {
        let answer = rig.ask(Method::GET, &path, &[("range", range)]).await;
        assert_eq!(answer.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(answer.body, expected.as_bytes(), "{range}");
        assert_eq!(answer.header("content-range"), content_range);
        assert_eq!(answer.header("content-length"), expected.len().to_string());
        assert_eq!(answer.header("docker-content-digest"), digest);
        assert!(!answer.cut);
    }
    let head = rig
        .ask(Method::HEAD, &path, &[("range", "bytes=2-4")])
        .await;
    assert_eq!(head.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.header("content-range"), "bytes 2-4/10");
    assert_eq!(head.header("content-length"), "3");
    assert!(head.body.is_empty());
    let whole = rig
        .ask(Method::GET, &path, &[("range", "bytes=1-2,5-6")])
        .await;
    assert_eq!((whole.status, whole.body.len()), (StatusCode::OK, 10));
    assert_eq!(rig.metrics.requests("blob", "partial"), 5);
}

/// Asserts a range that holds no byte is 416 with `Content-Range: bytes */size` and an error
/// document.
#[tokio::test]
async fn a_range_past_the_end_is_416() {
    let rig = Rig::new();
    let digest = rig.seed(b"0123456789").await;
    for range in ["bytes=10-", "bytes=10-20", "bytes=-0"] {
        let path = format!("/v2/a/blobs/{digest}");
        let answer = rig.ask(Method::GET, &path, &[("range", range)]).await;
        let (_, detail) =
            answer.assert_error(StatusCode::RANGE_NOT_SATISFIABLE, "RANGE_NOT_SATISFIABLE");
        assert_eq!(answer.header("content-range"), "bytes */10");
        assert_eq!(detail["size"], 10);
    }
    assert_eq!(rig.metrics.requests("blob", "range_not_satisfiable"), 3);
}

/// Asserts a blob whose bytes are not its digest is sent in full with its length and then its
/// body ends in an error, which the connection turns into a cut, and is counted.
#[tokio::test]
async fn a_blob_that_is_not_its_digest_ends_short() {
    let rig = Rig::new();
    let hex = hex_of(b"what the digest names");
    rig.put_under(&hex, b"what the store holds").await;
    let answer = rig.get(&format!("/v2/a/blobs/sha256:{hex}")).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(answer.cut);
    assert_eq!(rig.metrics.mismatches(), 1);
    // A ranged read of the same object is not checked: it is a part of the object.
    let part = rig
        .ask(
            Method::GET,
            &format!("/v2/a/blobs/sha256:{hex}"),
            &[("range", "bytes=0-3")],
        )
        .await;
    assert_eq!(
        (part.status, part.body, part.cut),
        (StatusCode::PARTIAL_CONTENT, b"what".to_vec(), false)
    );
    assert_eq!(rig.metrics.mismatches(), 1);
}

/// Asserts that every method but GET and HEAD is 405 `UNSUPPORTED` with `Allow: GET, HEAD`, on
/// every route and on a path that is none of them.
#[tokio::test]
async fn every_method_but_get_and_head_is_405() {
    let rig = Rig::new();
    let digest = rig.seed(b"x").await;
    let paths = [
        "/v2/".to_owned(),
        format!("/v2/a/blobs/{digest}"),
        format!("/v2/a/manifests/{digest}"),
        "/v2/a/blobs/uploads/".to_owned(),
        "/elsewhere".to_owned(),
    ];
    for method in [
        Method::PUT,
        Method::POST,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ] {
        for path in &paths {
            let answer = rig.ask(method.clone(), path, &[]).await;
            let (message, _) = answer.assert_error(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED");
            assert_eq!(answer.header("allow"), "GET, HEAD", "{method} {path}");
            assert!(message.contains("read-only"), "{message}");
        }
    }
    assert_eq!(rig.metrics.requests("ping", "not_allowed"), 5);
    assert_eq!(rig.metrics.requests("blob", "not_allowed"), 5);
    assert_eq!(rig.metrics.requests("manifest", "not_allowed"), 5);
    assert_eq!(rig.metrics.requests("other", "not_allowed"), 10);
    // Nothing was read.
    assert_eq!(rig.faulty.calls(Op::Get) + rig.faulty.calls(Op::Head), 0);
}

/// Asserts a path that is none of the three routes is 404 `UNSUPPORTED`, a request with an
/// `Authorization` header is served as one without, and none is ever challenged.
#[tokio::test]
async fn other_paths_are_404_and_nothing_is_challenged() {
    let rig = Rig::new();
    let digest = rig.seed(b"x").await;
    for path in ["/", "/v2", "/v2/_catalog", "/v2/a/tags/list", "/healthz"] {
        let answer = rig.get(path).await;
        let (message, _) = answer.assert_error(StatusCode::NOT_FOUND, "UNSUPPORTED");
        assert!(message.contains("only /v2/"), "{message}");
        assert!(!answer.has("www-authenticate"));
    }
    let token = [("authorization", "Basic dXNlcjpwYXNz")];
    let blob = rig
        .ask(Method::GET, &format!("/v2/a/blobs/{digest}"), &token)
        .await;
    assert_eq!(blob.status, StatusCode::OK);
    assert_eq!(blob.body, b"x");
    assert!(!blob.has("www-authenticate"));
    assert_eq!(rig.metrics.requests("other", "not_found"), 5);
}

/// Asserts a store that fails is 503 `UNAVAILABLE` when it cannot be reached and 500 `UNKNOWN`
/// otherwise, for a head and for a read, and that what the store said is not in the answer.
#[tokio::test]
async fn a_store_that_fails_is_503_or_500_without_its_message() {
    let rig = Rig::new();
    let manifest = rig.seed(MANIFEST.as_bytes()).await;
    let blob = rig.seed(b"x").await;
    rig.faulty.fail_next(Op::Head, 1);
    let answer = rig.get(&format!("/v2/a/blobs/{blob}")).await;
    answer.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    let secret = Error::Other("secret endpoint detail".to_owned());
    rig.faulty.fail_next_with(Op::Get, secret);
    let answer = rig.get(&format!("/v2/a/manifests/{manifest}")).await;
    answer.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
    assert!(!String::from_utf8_lossy(&answer.body).contains("secret"));
    assert_eq!(rig.metrics.requests("blob", "failed"), 1);
    assert_eq!(rig.metrics.requests("manifest", "failed"), 1);
}
