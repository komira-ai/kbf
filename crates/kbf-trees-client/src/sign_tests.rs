use std::time::Duration;

use super::*;

/// AWS's published SigV4 test suite: the secret of its key `AKIDEXAMPLE`.
const AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

/// The canonical request of AWS's `get-vanilla` and `post-vanilla` cases: `/`, the host
/// `example.amazonaws.com`, signed with `host;x-amz-date`, an empty body.
fn vanilla(method: &str) -> String {
    let headers = [
        ("host", "example.amazonaws.com"),
        ("x-amz-date", "20150830T123600Z"),
    ];
    canonical_request(method, "/", &headers, &payload_hash(b""))
}

/// Asserts the signature of AWS's published `get-vanilla` and `post-vanilla` cases, in the
/// scope `20150830/us-east-1/service`, is the one AWS publishes. Catches a canonical request,
/// string to sign or key derivation that differs from SigV4 anywhere: a node would refuse
/// every call.
#[test]
fn the_signature_of_aws_published_cases_is_aws_published_value() {
    let scope = ("us-east-1", "service");
    let stamp = "20150830T123600Z";
    assert_eq!(
        signature(AWS_SECRET, scope, stamp, &vanilla("GET")),
        "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
    );
    assert_eq!(
        signature(AWS_SECRET, scope, stamp, &vanilla("POST")),
        "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
    );
}

/// Asserts the canonical request is SigV4's: the method, the encoded path, an empty query,
/// each header as `name:value` with the value trimmed, a blank line, the signed names, and
/// the payload hash.
#[test]
fn the_canonical_request_lists_each_signed_header_and_the_payload() {
    let headers = [("host", " node:9600 "), ("x-amz-date", "20300101T000000Z")];
    assert_eq!(
        canonical_request("POST", "/a b/c", &headers, "abc"),
        "POST\n/a%20b/c\n\nhost:node:9600\nx-amz-date:20300101T000000Z\n\nhost;x-amz-date\nabc"
    );
}

/// Asserts the canonical path keeps the unreserved bytes and `/`, and writes every other
/// byte, of a multi-byte character too, as `%XX` in upper case; the gRPC path of a call is
/// kept as it is.
#[test]
fn the_canonical_path_encodes_every_byte_but_the_unreserved_ones() {
    let grpc = "/tierstore.trees.v1.TreesService/CreateTree";
    assert_eq!(canonical_path(grpc), grpc);
    assert_eq!(canonical_path("/A-z_0.9~"), "/A-z_0.9~");
    assert_eq!(canonical_path("/a+b=c%é"), "/a%2Bb%3Dc%25%C3%A9");
}

/// Asserts HMAC-SHA-256 against RFC 4231's case 2 (a short key) and case 6 (a key longer
/// than a block, which is hashed first). Catches a padding or a long-key rule that differs
/// from RFC 2104.
#[test]
fn hmac_is_rfc_4231s() {
    assert_eq!(
        hex::encode(hmac(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    let long_key = [0xaa_u8; 131];
    let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
    assert_eq!(
        hex::encode(hmac(&long_key, data)),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

/// Asserts `x-amz-date` is the UTC time as `YYYYMMDDTHHMMSSZ`: at the epoch, at AWS's test
/// time, on a leap day, on the last second of a year; and a time before the epoch is
/// spelled as the epoch.
#[test]
fn the_date_is_utc_in_sigv4s_form() {
    let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
    assert_eq!(amz_date(at(0)), "19700101T000000Z");
    assert_eq!(amz_date(at(1_440_938_160)), "20150830T123600Z");
    assert_eq!(amz_date(at(1_709_208_000)), "20240229T120000Z");
    assert_eq!(amz_date(at(1_735_689_599)), "20241231T235959Z");
    assert_eq!(
        amz_date(UNIX_EPOCH - Duration::from_secs(1)),
        "19700101T000000Z"
    );
    assert_eq!(civil(0), (1970, 1, 1));
    assert_eq!(civil(59), (1970, 3, 1));
}

/// Asserts the head of a signed call: the stamp of its time, the hash of its body, and an
/// `Authorization` that names the key, the scope `<date>/<region>/s3/aws4_request`, the three
/// signed headers, and the signature of the canonical request of `POST`, the path, the
/// authority as `host`, the hash and the stamp. Nothing in the head is the secret.
#[test]
fn a_call_is_signed_over_its_authority_path_body_and_time() {
    let at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let body = b"\0\0\0\0\x02\x0a\x00";
    let head = sign(("AKID", "s3cr3t"), "here", "node:9600", "/p/q", body, at);
    assert_eq!(head.stamp, "20270115T080000Z");
    assert_eq!(head.payload, payload_hash(body));
    let headers = [
        ("host", "node:9600"),
        ("x-amz-content-sha256", head.payload.as_str()),
        ("x-amz-date", "20270115T080000Z"),
    ];
    let canonical = canonical_request("POST", "/p/q", &headers, &head.payload);
    let expected = signature("s3cr3t", ("here", "s3"), "20270115T080000Z", &canonical);
    assert_eq!(
        head.authorization,
        format!(
            "AWS4-HMAC-SHA256 Credential=AKID/20270115/here/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={expected}"
        )
    );
    assert!(!format!("{head:?}").contains("s3cr3t"));
}

/// Asserts a scope's date is the stamp's first eight characters, and a stamp shorter than
/// that is taken whole rather than refused.
#[test]
fn a_short_stamp_is_its_own_date() {
    let auth = authorization("AKID", "r", "2030", "00");
    assert!(
        auth.contains("Credential=AKID/2030/r/s3/aws4_request"),
        "{auth}"
    );
    assert_eq!(
        signature("s", ("r", "s3"), "2030", "c").len(),
        64,
        "a signature is 32 bytes in hex"
    );
}
