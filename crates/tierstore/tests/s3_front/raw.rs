//! Requests signed by hand and sent with a plain HTTP client: what the client of kbf does not
//! send, and the answers as they are on the wire.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Client, Method, Response};
use tierstore::Backend;

use crate::signer::encode;
use crate::support::{BUCKET, Harness};

/// Sends a request with these headers (the host is the client's own) and this body.
pub async fn send(
    h: &Harness,
    method: &str,
    path: &str,
    query: &[(&str, &str)],
    headers: &[(String, String)],
    body: &[u8],
) -> Response {
    let pairs: Vec<String> = query
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name, b""), encode(value, b"")))
        .collect();
    let query = if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    };
    let url = format!("http://{}{}{query}", h.addr, encode(path, b"/"));
    let method = Method::from_bytes(method.as_bytes()).unwrap();
    let mut request = Client::new().request(method, url);
    for (name, value) in headers.iter().filter(|(name, _)| name != "host") {
        request = request.header(name.as_str(), value.as_str());
    }
    request.body(body.to_vec()).send().await.unwrap()
}

/// Signs a request for the key of the server at its fixed time, and sends it.
pub async fn call(
    h: &Harness,
    method: &str,
    key: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response {
    let path = format!("/{BUCKET}/{key}");
    let signed = h.signed(method, &path, query, headers, body);
    send(h, method, &path, query, &signed, body).await
}

/// The text of the body of an answer.
async fn text(response: Response) -> String {
    response.text().await.unwrap()
}

/// Asserts that a create-only put is answered 200 and again 412 with the S3 error document and
/// a request id, and that the first object is the one that is kept.
#[tokio::test]
async fn a_put_with_if_none_match_creates_once() {
    let h = Harness::start_fixed(|_| {}).await;
    let create = [("if-none-match", "*")];
    let first = call(&h, "PUT", "k", &[], &create, b"one").await;
    assert_eq!(first.status(), 200);
    let second = call(&h, "PUT", "k", &[], &create, b"two").await;
    assert_eq!(second.status(), 412);
    assert_eq!(second.headers()["content-type"], "application/xml");
    assert_eq!(second.headers()["x-amz-request-id"].len(), 16);
    let body = text(second).await;
    assert!(body.contains("<Code>PreconditionFailed</Code>"), "{body}");
    assert!(
        body.contains("<Resource>/tier-bucket/k</Resource>"),
        "{body}"
    );
    assert_eq!(h.hot_mem.get("k", None).await.unwrap(), "one");
}

/// Asserts that a put with `If-Match` of another tag is 412 and of the object's own tag is 501
/// (it cannot be replaced), and that the object is not changed by either.
#[tokio::test]
async fn a_put_with_if_match_is_never_a_replacement() {
    let h = Harness::start_fixed(|_| {}).await;
    call(&h, "PUT", "k", &[], &[("if-none-match", "*")], b"one").await;
    let head = call(&h, "HEAD", "k", &[], &[], b"").await;
    let tag = head.headers()["etag"].to_str().unwrap().to_owned();
    let other = [("if-match", "\"0000\"")];
    let refused = call(&h, "PUT", "k", &[], &other, b"two").await;
    assert_eq!(refused.status(), 412);
    let own = [("if-match", tag.as_str())];
    let kept = call(&h, "PUT", "k", &[], &own, b"two").await;
    assert_eq!(kept.status(), 501);
    assert_eq!(h.hot_mem.get("k", None).await.unwrap(), "one");
}

/// Asserts that a head answers the length, the tag and the time of an object with no body,
/// that a ranged get answers 206 with the range and the bytes, and that a range that starts
/// past the end is 416 with the size.
#[tokio::test]
async fn a_head_and_a_range_are_answered_as_s3_answers() {
    let h = Harness::start_fixed(|_| {}).await;
    let create = [("if-none-match", "*")];
    call(&h, "PUT", "k", &[], &create, b"hello world").await;
    let head = call(&h, "HEAD", "k", &[], &[], b"").await;
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], "11");
    assert_eq!(head.headers()["accept-ranges"], "bytes");
    let tag = head.headers()["etag"].to_str().unwrap();
    assert!(tag.starts_with('"'));
    let modified = head.headers()["last-modified"].to_str().unwrap();
    assert!(modified.ends_with(" GMT"));
    assert_eq!(text(head).await, "");
    let from_six = [("range", "bytes=6-")];
    let part = call(&h, "GET", "k", &[], &from_six, b"").await;
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], "bytes 6-10/11");
    assert_eq!(text(part).await, "world");
    let from_eleven = [("range", "bytes=11-")];
    let past = call(&h, "GET", "k", &[], &from_eleven, b"").await;
    assert_eq!(past.status(), 416);
    assert_eq!(past.headers()["content-range"], "bytes */11");
    let missing = call(&h, "GET", "absent", &[], &[], b"").await;
    assert_eq!(missing.status(), 404);
    assert!(text(missing).await.contains("<Code>NoSuchKey</Code>"));
}

/// Asserts that a request with no signature, with an authorization that is not SigV4, or
/// signed for another time than the server's by more than 15 minutes is 403 with the S3 code,
/// and that a body that is not the one the signature covers is 400.
#[tokio::test]
async fn requests_that_are_not_properly_signed_are_refused() {
    let h = Harness::start_fixed(|_| {}).await;
    let url = format!("http://{}/tier-bucket/k", h.addr);
    let unsigned = Client::new().get(&url).send().await.unwrap();
    assert_eq!(unsigned.status(), 403);
    assert!(text(unsigned).await.contains("<Code>AccessDenied</Code>"));
    let basic = Client::new()
        .get(&url)
        .header("authorization", "Basic eDp5");
    let basic = basic.send().await.unwrap();
    assert_eq!(basic.status(), 403);
    assert!(text(basic).await.contains("AuthorizationHeaderMalformed"));
    let hour_early = "20270115T070000Z";
    let signed = h.signed_at(hour_early, "GET", "/tier-bucket/k", &[], &[], b"");
    let skewed = send(&h, "GET", "/tier-bucket/k", &[], &signed, b"").await;
    assert_eq!(skewed.status(), 403);
    assert!(text(skewed).await.contains("RequestTimeTooSkewed"));
    let signed = h.signed("PUT", "/tier-bucket/k", &[], &[], b"abc");
    let forged = send(&h, "PUT", "/tier-bucket/k", &[], &signed, b"abd").await;
    assert_eq!(forged.status(), 400);
    assert!(text(forged).await.contains("XAmzContentSHA256Mismatch"));
    assert!(h.hot_mem.get("k", None).await.is_err());
}

/// Asserts that the health and readiness probes need no signature, and that a bucket the server
/// does not serve, a parameter it does not know and a listing with a delimiter are refused with
/// 404 and 501.
#[tokio::test]
async fn the_probes_are_open_and_the_unknown_is_refused() {
    let h = Harness::start_fixed(|_| {}).await;
    for path in ["healthz", "readyz"] {
        let url = format!("http://{}/{path}", h.addr);
        let probe = Client::new().get(url).send().await.unwrap();
        assert_eq!(probe.status(), 200);
    }
    let signed = h.signed("GET", "/other/k", &[], &[], b"");
    let other = send(&h, "GET", "/other/k", &[], &signed, b"").await;
    assert_eq!(other.status(), 404);
    assert!(text(other).await.contains("<Code>NoSuchBucket</Code>"));
    let versioned = call(&h, "GET", "k", &[("versionId", "1")], &[], b"").await;
    assert_eq!(versioned.status(), 501);
    let path = format!("/{BUCKET}");
    let query = [("list-type", "2"), ("delimiter", "/")];
    let signed = h.signed("GET", &path, &query, &[], b"");
    let delimited = send(&h, "GET", &path, &query, &signed, b"").await;
    assert_eq!(delimited.status(), 501);
}

/// The base64 of the big-endian CRC-32 of `bytes`, the value of an `x-amz-checksum-crc32` header.
fn crc32_of(bytes: &[u8]) -> String {
    STANDARD.encode(crc32fast::hash(bytes).to_be_bytes())
}

/// Asserts that a put whose body is not the one an `x-amz-checksum-crc32` header claims is 400
/// `BadDigest` and stores nothing, that a put whose body is the one is stored, that a
/// `Content-MD5` that is not the base64 of an MD5 is 400 `InvalidDigest`, and that the checksum of
/// an algorithm the server does not compute is 501.
#[tokio::test]
async fn a_checksum_that_does_not_match_is_400_and_nothing_is_stored() {
    let h = Harness::start_fixed(|_| {}).await;
    let (wrong, right) = (crc32_of(b"other"), crc32_of(b"hello"));
    let bad = [
        ("if-none-match", "*"),
        ("x-amz-checksum-crc32", wrong.as_str()),
    ];
    let refused = call(&h, "PUT", "k", &[], &bad, b"hello").await;
    assert_eq!(refused.status(), 400);
    let body = text(refused).await;
    assert!(body.contains("<Code>BadDigest</Code>"), "{body}");
    assert!(h.hot_mem.get("k", None).await.is_err());
    let good = [
        ("if-none-match", "*"),
        ("x-amz-checksum-crc32", right.as_str()),
    ];
    let stored = call(&h, "PUT", "k", &[], &good, b"hello").await;
    assert_eq!(stored.status(), 200);
    assert_eq!(h.hot_mem.get("k", None).await.unwrap(), "hello");
    let malformed = [("content-md5", "!!!")];
    let invalid = call(&h, "PUT", "j", &[], &malformed, b"hello").await;
    assert_eq!(invalid.status(), 400);
    assert!(text(invalid).await.contains("<Code>InvalidDigest</Code>"));
    let crc64 = [("x-amz-checksum-crc64nvme", "AAAAAAAAAAA=")];
    let unsupported = call(&h, "PUT", "j", &[], &crc64, b"hello").await;
    assert_eq!(unsupported.status(), 501);
    assert!(h.hot_mem.get("j", None).await.is_err());
}

/// Asserts that a body of the limit is taken and a longer one is 413.
#[tokio::test]
async fn a_body_over_the_limit_is_413() {
    let h = Harness::start_fixed(|config| config.max_request_bytes = 4).await;
    let create = [("if-none-match", "*")];
    let fits = call(&h, "PUT", "fits", &[], &create, b"1234").await;
    assert_eq!(fits.status(), 200);
    let over = call(&h, "PUT", "over", &[], &create, b"12345").await;
    assert_eq!(over.status(), 413);
    assert!(text(over).await.contains("EntityTooLarge"));
}
