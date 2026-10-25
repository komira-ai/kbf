//! The answers: header values, and the status and code of each failure of the store.

use std::time::Duration;

use bytes::Bytes;
use hyper::StatusCode;

use crate::Error;

use super::reply::{S3Error, answer, header_text};

/// Asserts that printable ASCII is kept, including the space, and everything else becomes `?`.
#[test]
fn a_header_value_keeps_printable_ascii_and_nothing_else() {
    let date = "Fri, 15 Jan 2027 08:00:00 GMT";
    assert_eq!(header_text(date), date);
    assert_eq!(header_text("é\u{7f}\n~"), "???~");
}

/// Asserts that an answer carries its status, its headers with the values made fit for a header,
/// and its body.
#[test]
fn an_answer_has_its_status_headers_and_body() {
    let etag = "\"é\"".to_owned();
    let headers = [("etag", etag), ("x-test", "a b".to_owned())];
    let body = Bytes::from_static(b"body");
    let made = answer(StatusCode::PARTIAL_CONTENT, &headers, body);
    assert_eq!(made.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(made.headers()["etag"], "\"?\"");
    assert_eq!(made.headers()["x-test"], "a b");
}

/// Converts an error of the store and checks the status, the code and the text.
fn check(error: Error, status: u16, code: &str) {
    let text = error.to_string();
    let failed = S3Error::from(error);
    let want = StatusCode::from_u16(status).unwrap();
    assert_eq!(failed.kind(), (want, code));
    assert_eq!(failed.message(), text);
}

/// Asserts the status and the S3 code of each failure of the store: a key that is not there,
/// one that is, a condition that does not hold, no room, a reserved key, a part a completion
/// names wrongly, a call the store does not do, a store that does not answer, and any other
/// failure.
#[test]
fn each_failure_of_the_store_has_its_status_and_code() {
    let k = || "k".to_owned();
    check(Error::NotFound(k()), 404, "NoSuchKey");
    check(Error::AlreadyExists(k()), 412, "PreconditionFailed");
    check(Error::PreconditionFailed(k()), 412, "PreconditionFailed");
    check(Error::Full(k()), 503, "SlowDown");
    check(Error::Reserved(k()), 403, "AccessDenied");
    check(Error::InvalidPart(k()), 400, "InvalidPart");
    check(Error::Unsupported(k()), 501, "NotImplemented");
    check(Error::Unavailable(k()), 503, "ServiceUnavailable");
    check(Error::Other(k()), 500, "InternalError");
}

/// Asserts that a request whose body stalled is `RequestTimeout` (400) with `Connection: close`
/// and the idle time in its text, that only it says so when asked, and that the answer sent
/// carries the header.
#[test]
fn a_request_that_stalled_is_a_timeout_that_closes_the_connection() {
    let stalled = S3Error::request_timeout(Duration::from_secs(30));
    assert_eq!(stalled.kind(), (StatusCode::BAD_REQUEST, "RequestTimeout"));
    assert!(stalled.message().contains("30s"), "{}", stalled.message());
    assert!(stalled.is_timeout());
    assert!(!S3Error::invalid("x").is_timeout());
    let sent = stalled.into_answer("/b/k", "ID");
    assert_eq!(sent.status(), StatusCode::BAD_REQUEST);
    assert_eq!(sent.headers()["connection"], "close");
}

/// Asserts the answers to a hash that does not match: the hash the signature covers is
/// `XAmzContentSHA256Mismatch`, any checksum header is `BadDigest` naming the header, and a
/// checksum header that is not a hash is `InvalidDigest` naming the header.
#[test]
fn a_hash_that_does_not_match_is_answered_by_the_header_it_was_claimed_in() {
    let payload = S3Error::mismatch("x-amz-content-sha256");
    let kind = (StatusCode::BAD_REQUEST, "XAmzContentSHA256Mismatch");
    assert_eq!(payload.kind(), kind);
    assert!(!payload.is_timeout());
    let bad = S3Error::mismatch("content-md5");
    assert_eq!(bad.kind(), (StatusCode::BAD_REQUEST, "BadDigest"));
    assert!(bad.message().contains("content-md5"));
    let invalid = S3Error::invalid_digest("x-amz-checksum-crc32");
    assert_eq!(invalid.kind(), (StatusCode::BAD_REQUEST, "InvalidDigest"));
    assert!(invalid.message().contains("x-amz-checksum-crc32"));
}
