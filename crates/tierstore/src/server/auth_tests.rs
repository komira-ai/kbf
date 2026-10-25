//! The check of the SigV4 signature of a request.

use std::sync::Arc;

use hyper::header::HeaderValue;
use hyper::http::request::Parts;
use hyper::{Method, Request};
use sha2::{Digest, Sha256};

use crate::TestClock;

use super::ServerConfig;
use super::auth::{
    MALFORMED, MISMATCH, SKEWED, UNKNOWN_KEY, UNSIGNED, WEAK, canonical_header, sign, verify,
};
use super::testing::{BUCKET, KEY_ID, NOW, SECRET};
use super::time::amz_date;

const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The example secret from the S3 SigV4 documentation, which its examples are signed with at
/// the time `DOC_NOW`.
const DOC_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

const DOC_NOW: u64 = 1_369_353_600_000;

const MINUTE: u64 = 60_000;

fn server_config(secret: &str, now: u64) -> ServerConfig {
    let clock = Arc::new(TestClock::new());
    clock.set(now);
    ServerConfig::new(BUCKET, KEY_ID, secret, clock)
}

/// The key and secret of the SigV4 documentation, at the time of its examples.
fn doc_config() -> ServerConfig {
    let mut config = server_config(DOC_SECRET, DOC_NOW);
    config.access_key_id = "AKIAIOSFODNN7EXAMPLE".to_owned();
    config
}

fn parts(method: &str, uri: &str, headers: &[(&str, &str)]) -> Parts {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(()).unwrap().into_parts().0
}

/// The parts of a request signed for the rig, then changed by `change`.
fn changed(change: impl FnOnce(&mut Parts)) -> Parts {
    let config = server_config(SECRET, NOW);
    let extra = [("range", "bytes=0-1")];
    let signed = sign(&config, "GET", "/b/k?prefix=a", &extra, b"");
    let mut parts = signed.into_parts().0;
    change(&mut parts);
    parts
}

fn set(parts: &mut Parts, name: &'static str, value: &str) {
    parts
        .headers
        .insert(name, HeaderValue::from_str(value).unwrap());
}

/// The text of the `Authorization` header of `parts`.
fn authorization(parts: &Parts) -> String {
    parts.headers["authorization"].to_str().unwrap().to_owned()
}

/// Asserts that the two worked examples of the SigV4 documentation, a GET Object with a
/// `Range` header and a GET Bucket (List Objects) with a query, are accepted: the canonical
/// request, the string to sign and the key derivation are the documented ones.
#[test]
fn the_documented_examples_verify() {
    let get_auth = concat!(
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, ",
        "SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, ",
        "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
    let get = parts(
        "GET",
        "/test.txt",
        &[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY),
            ("x-amz-date", "20130524T000000Z"),
            ("authorization", get_auth),
        ],
    );
    assert_eq!(verify(&doc_config(), &get), Ok(EMPTY.to_owned()));
    let list_auth = concat!(
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, ",
        "SignedHeaders=host;x-amz-content-sha256;x-amz-date, ",
        "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
    );
    let list = parts(
        "GET",
        "/?prefix=J&max-keys=2",
        &[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY),
            ("x-amz-date", "20130524T000000Z"),
            ("authorization", list_auth),
        ],
    );
    assert_eq!(verify(&doc_config(), &list), Ok(EMPTY.to_owned()));
}

/// Asserts that the canonical form of a header has its values trimmed, the runs of white space
/// inside them folded to one space, and the values of a repeated header joined by commas, and
/// that a header the request does not have has no canonical form.
#[test]
fn a_header_is_canonical_with_its_values_trimmed_folded_and_joined() {
    let request = parts("GET", "/", &[("x-test", "  a   b "), ("x-test", "c")]);
    let canonical = canonical_header(&request, "x-test");
    assert_eq!(canonical, Some("x-test:a b,c\n".to_owned()));
    assert_eq!(canonical_header(&request, "x-other"), None);
}

/// Asserts that a request signed as a client signs it is accepted, with the hash of its body
/// given back, whatever white space and repeated values its headers have.
#[test]
fn a_signed_request_is_accepted_and_gives_its_payload_hash() {
    let config = server_config(SECRET, NOW);
    let extra = [("x-test", "  a   b "), ("x-test", "c")];
    let signed = sign(&config, "PUT", "/b/a%20b?x=1&y=%2F", &extra, b"hello");
    let parts = signed.into_parts().0;
    let hash = hex::encode(Sha256::digest(b"hello"));
    assert_eq!(verify(&config, &parts), Ok(hash));
}

/// Asserts that a change to anything the signature covers is refused as a signature that does
/// not match: the path, the query, the method, a signed header, the host, the signature
/// itself, a signature that is not hex, and a request signed with another secret.
#[test]
fn a_change_to_what_is_signed_is_refused() {
    let config = server_config(SECRET, NOW);
    let refused = |parts: Parts| verify(&config, &parts).unwrap_err();
    let path = changed(|parts| parts.uri = "/b/other?prefix=a".parse().unwrap());
    assert_eq!(refused(path), MISMATCH);
    let query = changed(|parts| parts.uri = "/b/k?prefix=b".parse().unwrap());
    assert_eq!(refused(query), MISMATCH);
    let method = changed(|parts| parts.method = Method::PUT);
    assert_eq!(refused(method), MISMATCH);
    let range = changed(|parts| set(parts, "range", "bytes=0-2"));
    assert_eq!(refused(range), MISMATCH);
    let host = changed(|parts| set(parts, "host", "other.test"));
    assert_eq!(refused(host), MISMATCH);
    let signature = changed(|parts| {
        let text = authorization(parts).replace("Signature=", "Signature=00");
        set(parts, "authorization", &text);
    });
    assert_eq!(refused(signature), MISMATCH);
    let not_hex = changed(|parts| {
        let text = authorization(parts).replace("Signature=", "Signature=zz");
        set(parts, "authorization", &text);
    });
    assert_eq!(refused(not_hex), MISMATCH);
    let other = server_config("another-secret", NOW);
    let signed_by_other = sign(&other, "GET", "/b/k", &[], b"").into_parts().0;
    assert_eq!(refused(signed_by_other), MISMATCH);
}

/// Asserts that a request with no `Authorization` header is `AccessDenied` before anything else
/// is looked at.
#[test]
fn a_request_without_a_signature_is_refused() {
    let unsigned = parts("GET", "/b/k", &[("host", "tier.test")]);
    let config = server_config(SECRET, NOW);
    assert_eq!(verify(&config, &unsigned), Err(UNSIGNED));
}

/// Asserts that an `Authorization` header that is not a SigV4 one is malformed: another scheme,
/// a field without `=`, a missing field, a credential scope with too few or too many parts or
/// for another service or terminator.
#[test]
fn a_header_that_is_not_sigv4_is_malformed() {
    let signed = "SignedHeaders=host;x-amz-content-sha256;x-amz-date";
    let with =
        |scope: &str| format!("AWS4-HMAC-SHA256 Credential={scope}, {signed}, Signature=abcdef");
    let scope = "AKIDEXAMPLE/20270115/us-east-1/s3/aws4_request";
    let cases = [
        "Basic abc".to_owned(),
        "AWS4-HMAC-SHA256 garbage".to_owned(),
        format!("AWS4-HMAC-SHA256 {signed}, Signature=ab"),
        format!("AWS4-HMAC-SHA256 Credential={scope}, Signature=ab"),
        format!("AWS4-HMAC-SHA256 Credential={scope}, {signed}"),
        with("AKIDEXAMPLE/20270115"),
        with("AKIDEXAMPLE/20270115/us-east-1/s3/aws4_request/more"),
        with("AKIDEXAMPLE/20270115/us-east-1/ec2/aws4_request"),
        with("AKIDEXAMPLE/20270115/us-east-1/s3/aws5_request"),
    ];
    for text in cases {
        let request = changed(|parts| set(parts, "authorization", &text));
        let refusal = verify(&server_config(SECRET, NOW), &request);
        assert_eq!(refusal, Err(MALFORMED), "{text}");
    }
}

/// Asserts that an unknown field of the header is ignored, so a request that has one is judged
/// by its signature, and that the key id and the region of the credential must be the
/// server's.
#[test]
fn the_key_id_and_the_region_must_be_the_servers() {
    let config = server_config(SECRET, NOW);
    let extra = changed(|parts| {
        let text = authorization(parts).replace("SignedHeaders=", "Other=1, SignedHeaders=");
        set(parts, "authorization", &text);
    });
    assert!(verify(&config, &extra).is_ok());
    let key = changed(|parts| {
        let text = authorization(parts).replace(KEY_ID, "AKIDOTHER");
        set(parts, "authorization", &text);
    });
    assert_eq!(verify(&config, &key), Err(UNKNOWN_KEY));
    let region = changed(|parts| {
        let text = authorization(parts).replace("/us-east-1/", "/eu-west-1/");
        set(parts, "authorization", &text);
    });
    assert_eq!(verify(&config, &region), Err(UNKNOWN_KEY));
}

/// Asserts that a request whose signature leaves out the host, the payload hash or the date is
/// refused, however good the signature is.
#[test]
fn the_host_the_payload_hash_and_the_date_must_be_signed() {
    for left_out in ["host;", ";x-amz-content-sha256", ";x-amz-date"] {
        let weak = changed(|parts| {
            let text = authorization(parts).replace(left_out, "");
            set(parts, "authorization", &text);
        });
        let refusal = verify(&server_config(SECRET, NOW), &weak);
        assert_eq!(refusal, Err(WEAK), "{left_out}");
    }
}

/// Asserts that a request up to 15 minutes from the server's time either way is accepted, and
/// one further is `RequestTimeTooSkewed`.
#[test]
fn a_request_must_be_within_fifteen_minutes_of_the_servers_time() {
    let config = server_config(SECRET, NOW);
    let signed = sign(&config, "GET", "/b/k", &[], b"").into_parts().0;
    for later in [0, 14 * MINUTE, 15 * MINUTE] {
        let after = server_config(SECRET, NOW + later);
        assert!(verify(&after, &signed).is_ok(), "{later}");
        let before = server_config(SECRET, NOW - later);
        assert!(verify(&before, &signed).is_ok(), "{later}");
    }
    let too_late = server_config(SECRET, NOW + 16 * MINUTE);
    assert_eq!(verify(&too_late, &signed), Err(SKEWED));
    let too_early = server_config(SECRET, NOW - 16 * MINUTE);
    assert_eq!(verify(&too_early, &signed), Err(SKEWED));
}

/// Asserts that a request is malformed when the day of its credential is not the day of its
/// date, and when it lacks the date header or the payload hash header it says it signed.
#[test]
fn the_date_and_the_payload_hash_must_be_there_and_agree() {
    let config = server_config(SECRET, NOW);
    let day = amz_date(NOW)[..8].to_owned();
    let other_day = changed(|parts| {
        let text = authorization(parts).replace(&format!("/{day}/"), "/20000101/");
        set(parts, "authorization", &text);
    });
    assert_eq!(verify(&config, &other_day), Err(MALFORMED));
    let no_date = changed(|parts| {
        parts.headers.remove("x-amz-date");
    });
    assert_eq!(verify(&config, &no_date), Err(MALFORMED));
    let no_hash = changed(|parts| {
        parts.headers.remove("x-amz-content-sha256");
    });
    assert_eq!(verify(&config, &no_hash), Err(MALFORMED));
}

/// Asserts that a request is malformed when a header it says it signed is missing, or its path
/// or its query is not UTF-8 once decoded: there is no canonical request to check.
#[test]
fn a_request_with_no_canonical_form_is_malformed() {
    let config = server_config(SECRET, NOW);
    let no_range = changed(|parts| {
        parts.headers.remove("range");
    });
    assert_eq!(verify(&config, &no_range), Err(MALFORMED));
    let bad_path = changed(|parts| parts.uri = "/b/%FF".parse().unwrap());
    assert_eq!(verify(&config, &bad_path), Err(MALFORMED));
    let bad_query = changed(|parts| parts.uri = "/b/k?prefix=%FF".parse().unwrap());
    assert_eq!(verify(&config, &bad_query), Err(MALFORMED));
}
