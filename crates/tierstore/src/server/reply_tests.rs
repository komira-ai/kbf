//! The answers: header values, and the status and code of each failure of the store.

use bytes::Bytes;
use hyper::StatusCode;

use crate::Error;

use super::reply::{S3Error, answer, header_text};
use super::uploads::Rejected;

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
/// one that is, a condition that does not hold, no room, a reserved key, a store that does not
/// answer, and any other failure.
#[test]
fn each_failure_of_the_store_has_its_status_and_code() {
    let k = || "k".to_owned();
    check(Error::NotFound(k()), 404, "NoSuchKey");
    check(Error::AlreadyExists(k()), 412, "PreconditionFailed");
    check(Error::PreconditionFailed(k()), 412, "PreconditionFailed");
    check(Error::Full(k()), 503, "SlowDown");
    check(Error::Reserved(k()), 403, "AccessDenied");
    check(Error::Unavailable(k()), 503, "ServiceUnavailable");
    check(Error::Other(k()), 500, "InternalError");
}

/// Asserts that a part the server cannot take is `NoSuchUpload`, `SlowDown` where it can be sent
/// again later, or `EntityTooLarge` where it never can be taken.
#[test]
fn a_rejected_part_has_its_status_and_code() {
    let missing = S3Error::from(Rejected::NoSuchUpload);
    assert_eq!(missing.kind(), (StatusCode::NOT_FOUND, "NoSuchUpload"));
    let full = S3Error::from(Rejected::Full);
    assert_eq!(full.kind(), (StatusCode::SERVICE_UNAVAILABLE, "SlowDown"));
    let too_large = S3Error::from(Rejected::TooLarge);
    assert_eq!(too_large.kind(), (StatusCode::BAD_REQUEST, "EntityTooLarge"));
}
