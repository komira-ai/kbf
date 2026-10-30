//! The answers: header values, and the status and code of each failure of the store.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use http_body_util::BodyExt as _;
use hyper::StatusCode;
use hyper::body::Body as _;

use crate::{Error, ReadBody};

use super::reply::{AnswerBody, S3Error, answer, header_text, streamed_answer};

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

/// A body of the chunks and errors given, in order.
fn chunks(items: Vec<Result<&'static [u8], Error>>) -> ReadBody {
    let items = items
        .into_iter()
        .map(|item| item.map(Bytes::from_static))
        .collect::<Vec<_>>();
    stream::iter(items).boxed()
}

/// The frames of `body` to its end: the bytes of each, or the error that ended it.
async fn frames(mut body: AnswerBody) -> Vec<Result<Bytes, Error>> {
    let mut seen = Vec::new();
    while let Some(frame) = body.frame().await {
        seen.push(frame.map(|frame| frame.into_data().expect("data")));
    }
    seen
}

/// Asserts that a streamed answer gives its length to the connection (so a length header is
/// sent), gives the chunks as they are, skips an empty chunk, and ends cleanly after exactly
/// that many bytes. The end is the stream's own: `is_end_stream` is false until then.
#[tokio::test]
async fn a_streamed_answer_of_the_length_given_ends_cleanly() {
    let body = chunks(vec![Ok(b"abc"), Ok(b""), Ok(b"de")]);
    let made = streamed_answer(StatusCode::OK, &[], 5, body);
    let body = made.into_body();
    assert_eq!(body.size_hint().exact(), Some(5));
    assert!(!body.is_end_stream());
    let seen = frames(body).await;
    assert_eq!(
        seen,
        vec![
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"de"))
        ]
    );
}

/// Asserts that a streamed answer whose stream ends before the length it gave, gives more bytes
/// than it gave, or ends with an error is an error of the body, which comes once and ends the
/// body: the connection is cut and the client cannot take the answer for whole. The last chunk of
/// the stream is held until the stream has ended well, so a body that fails after its last byte
/// never gives that byte (the connection would otherwise count the bytes and end the answer
/// before the error is read): the error is the only item where the last chunk would be.
#[tokio::test]
async fn a_streamed_answer_that_is_short_long_or_failed_ends_with_an_error() {
    let short = streamed_answer(StatusCode::OK, &[], 5, chunks(vec![Ok(b"abc")]));
    let seen = frames(short.into_body()).await;
    assert_eq!(seen.len(), 1);
    assert!(matches!(&seen[0], Err(Error::Other(text)) if text.contains("2 bytes short")));

    let long = streamed_answer(StatusCode::OK, &[], 2, chunks(vec![Ok(b"ab"), Ok(b"c")]));
    let seen = frames(long.into_body()).await;
    assert_eq!(seen.len(), 1);
    assert!(matches!(&seen[0], Err(Error::Other(text)) if text.contains("more bytes")));

    let failed = Error::Unavailable("gone".to_owned());
    let late = chunks(vec![Ok(b"ab"), Ok(b"cd"), Err(failed.clone()), Ok(b"ef")]);
    let seen = frames(streamed_answer(StatusCode::OK, &[], 6, late).into_body()).await;
    assert_eq!(
        seen,
        vec![Ok(Bytes::from_static(b"ab")), Err(failed.clone())]
    );

    let after = chunks(vec![Ok(b"ab"), Err(failed.clone())]);
    let seen = frames(streamed_answer(StatusCode::OK, &[], 2, after).into_body()).await;
    assert_eq!(seen, vec![Err(failed)]);
}
