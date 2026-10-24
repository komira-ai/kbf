//! What the server reads of a request: the path, the query, the body and its hash.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::Full;
use hyper::StatusCode;
use hyper::body::{Body, Frame};
use hyper::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};

use super::request::{Query, Target, check_payload, header, query_pairs, read_body, resolve};

fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let own = |(name, value): &(&str, &str)| (name.to_string(), value.to_string());
    pairs.iter().map(own).collect()
}

/// A body that fails when it is read.
struct Broken;

impl Body for Broken {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        Poll::Ready(Some(Err(std::io::Error::other("cut"))))
    }
}

/// Asserts that a path names the service, the bucket with or without a closing slash, or an
/// object whose key is the decoded rest of the path.
#[test]
fn a_path_names_the_service_the_bucket_or_an_object() {
    assert_eq!(resolve("bkt", "/").unwrap(), Target::Root);
    assert_eq!(resolve("bkt", "/bkt").unwrap(), Target::Bucket);
    assert_eq!(resolve("bkt", "/bkt/").unwrap(), Target::Bucket);
    let nested = resolve("bkt", "/bkt/a/b").unwrap();
    assert_eq!(nested, Target::Object("a/b".to_owned()));
    let encoded = resolve("bkt", "/bkt/a%2Fb%20c").unwrap();
    assert_eq!(encoded, Target::Object("a/b c".to_owned()));
}

/// Asserts that a path of another bucket is `NoSuchBucket` and a path that is not UTF-8 once
/// decoded is `InvalidURI`.
#[test]
fn a_path_of_another_bucket_or_of_bad_bytes_is_refused() {
    let other = resolve("bkt", "/else/k").unwrap_err();
    assert_eq!(other.kind(), (StatusCode::NOT_FOUND, "NoSuchBucket"));
    assert!(other.message().contains("else"));
    let bad = resolve("bkt", "/bkt/%FF").unwrap_err();
    assert_eq!(bad.kind(), (StatusCode::BAD_REQUEST, "InvalidURI"));
}

/// Asserts that a query is split at `&` and `=`, decoded, and has no pair for an empty piece,
/// and that a name alone has the empty value.
#[test]
fn a_query_is_split_and_decoded() {
    assert_eq!(query_pairs(None), Some(Vec::new()));
    assert_eq!(query_pairs(Some("")), Some(Vec::new()));
    let pairs = query_pairs(Some("a=1&&b=x%2Fy+z&flag&c=%41")).unwrap();
    let want = owned(&[("a", "1"), ("b", "x/y+z"), ("flag", ""), ("c", "A")]);
    assert_eq!(pairs, want);
}

/// Asserts that a query whose name or value is not UTF-8 once decoded is not read.
#[test]
fn a_query_that_is_not_utf8_is_refused() {
    assert_eq!(query_pairs(Some("a=%FF")), None);
    assert_eq!(query_pairs(Some("%FF=1")), None);
    let refused = Query::parse(Some("a=%FF")).unwrap_err();
    assert_eq!(refused.kind(), (StatusCode::BAD_REQUEST, "InvalidURI"));
}

/// Asserts that the first of equal names answers, that a name is found with or without a value,
/// and that the any and only checks say what they are asked.
#[test]
fn a_query_answers_what_it_has() {
    let query = Query::parse(Some("prefix=a%20b&list-type=2&prefix=second")).unwrap();
    assert_eq!(query.get("prefix"), Some("a b"));
    assert_eq!(query.get("absent"), None);
    assert!(query.has("list-type"));
    assert!(!query.has("max-keys"));
    assert!(query.has_any(&["max-keys", "prefix"]));
    assert!(!query.has_any(&["max-keys", "uploads"]));
    assert!(query.only(&["prefix", "list-type"]));
    assert!(!query.only(&["prefix"]));
}

/// Asserts that a header is read as text when it is, and not when it is absent or not text.
#[test]
fn a_header_is_text_or_nothing() {
    let mut headers = HeaderMap::new();
    headers.insert("x-a", HeaderValue::from_static("v"));
    headers.insert("x-b", HeaderValue::from_bytes(b"\xff").unwrap());
    assert_eq!(header(&headers, "x-a"), Some("v"));
    assert_eq!(header(&headers, "x-b"), None);
    assert_eq!(header(&headers, "x-c"), None);
}

/// Asserts that a body is read whole when it has at most the limit, that a longer one is
/// `EntityTooLarge`, and that one that fails is `IncompleteBody`.
#[tokio::test]
async fn a_body_is_read_up_to_its_limit() {
    let four = Full::new(Bytes::from_static(b"abcd"));
    let read = read_body(four, 4).await.unwrap();
    assert_eq!(read, Bytes::from_static(b"abcd"));
    let five = Full::new(Bytes::from_static(b"abcde"));
    let over = read_body(five, 4).await.unwrap_err();
    let too_large = (StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge");
    assert_eq!(over.kind(), too_large);
    assert!(over.message().contains("4 bytes"));
    let cut = read_body(Broken, 4).await.unwrap_err();
    assert_eq!(cut.kind(), (StatusCode::BAD_REQUEST, "IncompleteBody"));
}

/// Asserts that a body is accepted with the hash it has in either case, or when the payload is
/// unsigned, and refused with another hash, and with a claim of a signed stream.
#[test]
fn a_body_is_checked_against_the_hash_the_signature_covers() {
    let body = Bytes::from_static(b"abc");
    let digest = hex::encode(Sha256::digest(&body));
    assert!(check_payload("UNSIGNED-PAYLOAD", &body).is_ok());
    assert!(check_payload(&digest, &body).is_ok());
    assert!(check_payload(&digest.to_uppercase(), &body).is_ok());
    let other = hex::encode(Sha256::digest(b"abd"));
    let wrong = check_payload(&other, &body).unwrap_err();
    let mismatch = (StatusCode::BAD_REQUEST, "XAmzContentSHA256Mismatch");
    assert_eq!(wrong.kind(), mismatch);
    let claim = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
    let streaming = check_payload(claim, &body).unwrap_err();
    let not_done = (StatusCode::NOT_IMPLEMENTED, "NotImplemented");
    assert_eq!(streaming.kind(), not_done);
}
