use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::sign::sign;

const PATH: &str = "/tierstore.trees.v1.TreesService/GetPlacement";

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

/// The head of a call to `authority` signed at `at` with `key` in `region`, over `body`.
fn signed(key: (&str, &str), region: &str, at: SystemTime, body: &[u8]) -> http::request::Parts {
    let head = sign(key, region, "node:1", PATH, body, at);
    let (parts, ()) = http::Request::post(format!("http://node:1{PATH}"))
        .header("x-amz-date", head.stamp)
        .header("x-amz-content-sha256", head.payload)
        .header("authorization", head.authorization)
        .body(())
        .unwrap()
        .into_parts();
    parts
}

fn good() -> http::request::Parts {
    signed((KEY_ID, SECRET), REGION, now(), b"body")
}

/// `parts` with header `name` set to `value`, or removed when `value` is `None`.
fn with(
    mut parts: http::request::Parts,
    name: &'static str,
    value: Option<&str>,
) -> http::request::Parts {
    match value {
        Some(value) => {
            parts.headers.insert(name, value.parse().unwrap());
        }
        None => {
            parts.headers.remove(name);
        }
    }
    parts
}

/// Asserts a call signed with the fake's key, at a time within 15 minutes either side of
/// the fake's, over its body, passes; and with the `host` header in place of the authority.
#[test]
fn a_call_signed_with_the_key_passes() {
    assert_eq!(verify(&good(), b"body", now()), Ok(()));
    let skew = Duration::from_secs(14 * 60);
    let early = signed((KEY_ID, SECRET), REGION, now() - skew, b"body");
    assert_eq!(verify(&early, b"body", now()), Ok(()));
    let late = signed((KEY_ID, SECRET), REGION, now() + skew, b"body");
    assert_eq!(verify(&late, b"body", now()), Ok(()));
    let mut by_header = good();
    by_header.uri = PATH.parse().unwrap();
    let by_header = with(by_header, "host", Some("node:1"));
    assert_eq!(verify(&by_header, b"body", now()), Ok(()));
}

/// Asserts each way a call fails the check is refused with the node's words: no
/// `Authorization`, a malformed one, another key, region or service, a signature that does not
/// cover the three headers, no time, a time 16 minutes off either side or not 16 characters,
/// a scope date that is not the time's, no payload hash, a signed header that is missing, a
/// wrong signature, a body that is not the hashed one.
#[test]
fn each_unsigned_or_missigned_call_is_refused_by_why() {
    let skew = Duration::from_secs(16 * 60);
    let auth =
        |parts: &http::request::Parts| header(&parts.headers, "authorization").unwrap().to_owned();
    let other_service = auth(&good()).replace("/s3/", "/sqs/");
    let weak = auth(&good()).replace("host;x-amz-content-sha256;x-amz-date", "host;x-amz-date");
    let extra = auth(&good()).replace(
        "host;x-amz-content-sha256;x-amz-date",
        "host;x-amz-content-sha256;x-amz-date;x-extra",
    );
    let other_date = auth(&good()).replace("/20270115/", "/20270116/");
    let cases: Vec<(http::request::Parts, &[u8], &str)> = vec![
        (
            with(good(), "authorization", None),
            b"body",
            "no Authorization header",
        ),
        (
            with(good(), "authorization", Some("Bearer x")),
            b"body",
            "a malformed Authorization header",
        ),
        (
            signed(("OTHER", SECRET), REGION, now(), b"body"),
            b"body",
            "an unknown key",
        ),
        (
            signed((KEY_ID, SECRET), "elsewhere", now(), b"body"),
            b"body",
            "an unknown key",
        ),
        (
            with(good(), "authorization", Some(&other_service)),
            b"body",
            "an unknown key",
        ),
        (
            with(good(), "authorization", Some(&weak)),
            b"body",
            "the signature does not cover host, x-amz-content-sha256 and x-amz-date",
        ),
        (with(good(), "x-amz-date", None), b"body", "no x-amz-date"),
        (
            signed((KEY_ID, SECRET), REGION, now() - skew, b"body"),
            b"body",
            "the time is too far from the node's",
        ),
        (
            signed((KEY_ID, SECRET), REGION, now() + skew, b"body"),
            b"body",
            "the time is too far from the node's",
        ),
        (
            with(good(), "x-amz-date", Some("20270115T080000")),
            b"body",
            "the time is too far from the node's",
        ),
        (
            with(good(), "authorization", Some(&other_date)),
            b"body",
            "the scope's date is not the time's",
        ),
        (
            with(good(), "x-amz-content-sha256", None),
            b"body",
            "no x-amz-content-sha256",
        ),
        (
            with(good(), "authorization", Some(&extra)),
            b"body",
            "a signed header is missing",
        ),
        (
            signed((KEY_ID, "wrong"), REGION, now(), b"body"),
            b"body",
            "the signature does not match",
        ),
        (
            good(),
            b"other",
            "the body is not the one the signature covers",
        ),
    ];
    for (parts, body, why) in cases {
        assert_eq!(verify(&parts, body, now()), Err(why));
    }
}

/// Asserts the `Authorization` reader takes the three fields in order and nothing else.
#[test]
fn an_authorization_header_is_read_field_by_field() {
    type Fields<'a> = (&'a str, &'a str, &'a str, &'a str, Vec<&'a str>, &'a str);
    fn read(text: &str) -> Option<Fields<'_>> {
        Authorization::parse(text)
            .map(|a| (a.id, a.date, a.region, a.service, a.signed, a.signature))
    }
    assert_eq!(
        read("AWS4-HMAC-SHA256 Credential=K/D/R/S/aws4_request, SignedHeaders=a;b, Signature=ff"),
        Some(("K", "D", "R", "S", vec!["a", "b"], "ff"))
    );
    for text in [
        "AWS4-HMAC-SHA256 Credential=K/D/R/S/other, SignedHeaders=a, Signature=ff",
        "AWS4-HMAC-SHA256 Credential=K/D/R/S/aws4_request, SignedHeaders=a, Signature=ff, More=1",
        "AWS4-HMAC-SHA256 Credential=K/D/R, SignedHeaders=a, Signature=ff",
        "AWS4-HMAC-SHA256 Credential=K/D/R/S/aws4_request, Signature=ff",
        "AWS4-HMAC-SHA256 Credential=K/D/R/S/aws4_request, SignedHeaders=a",
        "AWS4-HMAC-SHA256 SignedHeaders=a",
        "AWS4-HMAC-SHA256 ",
    ] {
        assert_eq!(read(text), None, "{text}");
    }
}

/// Asserts a call is decoded by its method, and a method the service does not have, or a
/// message that is not the method's request, is not.
#[test]
fn a_call_is_decoded_by_its_method() {
    let delete = DeleteTreeRequest {
        placement: "p1".to_owned(),
    };
    assert_eq!(
        decode_call("DeleteTree", &delete.encode_to_vec()),
        Some(Call::DeleteTree(delete))
    );
    assert_eq!(decode_call("Other", &[]), None);
    assert_eq!(decode_call("CreateTree", &[0xff]), None);
    assert_eq!(&frame(b"ab")[..], &[0, 0, 0, 0, 2, b'a', b'b']);
}
