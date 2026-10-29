//! What the server does with a presigned URL, through `Server::handle`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use crate::{Backend, Error, Op, Proxy, ProxyConfig, TestClock};

use super::auth::sign_as;
use super::presign::presign_url;
use super::presign::tests::{NOW, SECRET, access, grant, request};
use super::presign::{Grant, PresignKey};
use super::testing::{BUCKET, KEY_ID, Reply, Rig, Scripted, Step};
use super::{AccessKey, Server, ServerConfig};

const MEBIBYTE: u64 = 1 << 20;

/// A server with a presigning key whose bodies are at most 16 bytes, the time set by hand.
struct Fixture {
    rig: Rig,
    clock: Arc<TestClock>,
}

async fn fixture_with(change: impl FnOnce(&mut ServerConfig)) -> Fixture {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let shared = Arc::clone(&clock);
    let small = Grant {
        max_object_bytes: 16,
        ..grant()
    };
    let held = PresignKey::new("pack-presign", SECRET, small).unwrap();
    let made = Rig::with(MEBIBYTE, move |config| {
        config.clock = shared;
        config.presign_keys = vec![held];
        config.read_only_keys = vec![AccessKey::new("AKIDREADER", "reader-secret")];
        change(config);
    });
    Fixture {
        rig: made.await,
        clock,
    }
}

async fn fixture() -> Fixture {
    fixture_with(|_| {}).await
}

/// A presigned `PUT` URL for `key`, good for `secs`, signed by the presigning key.
fn put_url(key: &str, secs: u64) -> String {
    presign_url("PUT", &access("pack-presign"), &request(key, secs), &[]).unwrap()
}

/// The request for a URL, with a body and the headers `extra`.
fn request_for(
    method: &str,
    url: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Request<Full<Bytes>> {
    let (parts, ()) = {
        let mut builder = Request::builder().method(method);
        let rest = url.split_once("://").unwrap().1;
        let at = rest.find('/').unwrap();
        builder = builder.uri(&rest[at..]).header("host", &rest[..at]);
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts()
    };
    Request::from_parts(parts, Full::new(Bytes::copy_from_slice(body)))
}

fn refused(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

impl Fixture {
    async fn send(&self, method: &str, url: &str, body: &[u8]) -> Reply {
        self.rig.send_raw(request_for(method, url, &[], body)).await
    }

    async fn put(&self, url: &str, body: &[u8]) -> Reply {
        self.send("PUT", url, body).await
    }

    /// Whether any tier holds the key.
    async fn holds(&self, key: &str) -> bool {
        let hot = self.rig.hot_mem.get(key, None).await.is_ok();
        hot || self.rig.cold_mem.get(key, None).await.is_ok()
    }
}

/// Asserts that a presigned PUT stores the object, with the signature as the only credential, and
/// that the same URL used again is `PreconditionFailed` and leaves the first body: the first
/// complete PUT of a key wins. It catches a front that lets a second put replace the first.
#[tokio::test(start_paused = true)]
async fn a_presigned_put_succeeds_once_and_a_second_is_refused() {
    let f = fixture().await;
    let url = put_url("cas/b/build-1/nonce", 300);
    assert_eq!(f.put(&url, b"first").await.status, StatusCode::OK);
    assert_eq!(f.rig.get("cas/b/build-1/nonce", None).await.body, "first");
    let again = f.put(&url, b"second").await;
    assert_eq!(refused(&again), (412, "PreconditionFailed".to_owned()));
    let other = f.put(&put_url("cas/b/build-1/nonce", 200), b"third").await;
    assert_eq!(other.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(f.rig.get("cas/b/build-1/nonce", None).await.body, "first");
}

/// Asserts the expiry on the server: good before `X-Amz-Date + X-Amz-Expires`, `AccessDenied`
/// (request has expired) from it on, with nothing stored.
#[tokio::test(start_paused = true)]
async fn an_expired_url_stores_nothing() {
    let f = fixture().await;
    let url = put_url("cas/b/x", 60);
    f.clock.set(NOW + 59_999);
    let late = f.put(&url, b"early").await;
    assert_eq!(late.status, StatusCode::OK);
    let url = put_url("cas/b/y", 60);
    for after in [60_000, 60_001, 10 * 60_000] {
        f.clock.set(NOW + after);
        let reply = f.put(&url, b"late").await;
        assert_eq!(refused(&reply), (403, "AccessDenied".to_owned()), "{after}");
        assert!(reply.body.contains("expired"), "{}", reply.body);
    }
    assert!(!f.holds("cas/b/y").await);
}

/// Asserts that an expiry over the maximum of the server's flag, or over the key's ceiling, is
/// refused though the URL is signed right, and nothing is stored.
#[tokio::test(start_paused = true)]
async fn an_expiry_over_the_maximum_is_refused() {
    let f = fixture_with(|config| config.presign_max_expires = Duration::from_secs(120)).await;
    let ok = f.put(&put_url("cas/b/ok", 120), b"x").await;
    assert_eq!(ok.status, StatusCode::OK);
    for secs in [121, 600, 601, 3600, 604_800] {
        let reply = f.put(&put_url("cas/b/long", secs), b"x").await;
        assert_eq!(
            refused(&reply),
            (403, "AuthorizationQueryParametersError".to_owned())
        );
    }
    assert!(!f.holds("cas/b/long").await);
}

/// Asserts that a wrong signature and a changed key in the path are `SignatureDoesNotMatch`, that
/// the error document does not carry the signature or the query, and that nothing is stored.
#[tokio::test(start_paused = true)]
async fn a_wrong_signature_and_a_tampered_key_store_nothing() {
    let f = fixture().await;
    let url = put_url("cas/b/mine", 300);
    let signature = url.rsplit('=').next().unwrap().to_owned();
    let flipped = format!(
        "{}{}",
        &signature[..63],
        if signature.ends_with('0') { '1' } else { '0' }
    );
    let wrong = f.put(&url.replace(&signature, &flipped), b"x").await;
    assert_eq!(refused(&wrong), (403, "SignatureDoesNotMatch".to_owned()));
    assert!(!wrong.body.contains(&signature) && !wrong.body.contains(&flipped));
    assert!(!wrong.body.contains("X-Amz"), "{}", wrong.body);
    let moved = f
        .put(&url.replace("cas/b/mine", "cas/b/theirs"), b"x")
        .await;
    assert_eq!(refused(&moved), (403, "SignatureDoesNotMatch".to_owned()));
    let outside = f.put(&url.replace("cas/b/mine", "cas/c/mine"), b"x").await;
    assert_eq!(outside.status, StatusCode::FORBIDDEN);
    assert!(!f.holds("cas/b/mine").await && !f.holds("cas/b/theirs").await);
    assert!(!f.holds("cas/c/mine").await);
}

/// Asserts that a URL correctly signed for a key outside the prefix of the presigning key is
/// refused, after the path is decoded, with every spelling that would climb out of the prefix or
/// reach the keys of the proxy: nothing is stored. It catches a check on the raw path.
#[tokio::test(start_paused = true)]
async fn a_key_outside_the_prefix_is_refused_in_every_spelling() {
    let f = fixture().await;
    let denied = [
        "cas/c/x",
        "cas/b",
        "cas/bb/x",
        "cas/x",
        "x",
        "cas/b/../c/x",
        "cas/b/./x",
        "cas/b/..",
        "cas/b/...",
        "cas/b/..%2Fc/x",
        "_journal/p1/0000000000000001",
        "_tiering/usage",
        "_tiering/lease",
    ];
    for name in denied {
        let url = presign_url("PUT", &access("pack-presign"), &request(name, 300), &[]).unwrap();
        let reply = f.put(&url, b"X").await;
        assert_eq!(refused(&reply), (403, "AccessDenied".to_owned()), "{name}");
        assert!(!f.holds(name).await, "{name}");
    }
    // Spellings that only decode to a key outside the prefix: the path is signed as sent.
    for raw in [
        "cas/b/%2e%2e/c/x",
        "cas/b/%2E%2E/c/x",
        "cas/b/..%2Fc/x",
        "cas/b/%2e/x",
        "cas%2Fc%2Fx",
        "cas/b/x%00",
    ] {
        let url = presign_url(
            "PUT",
            &access("pack-presign"),
            &super::presign::PresignRequest {
                key: "placeholder",
                ..request("placeholder", 300)
            },
            &[],
        )
        .unwrap();
        let signed_raw = resign_with_raw_path(&url, raw);
        let reply = f.put(&signed_raw, b"X").await;
        assert_eq!(refused(&reply), (403, "AccessDenied".to_owned()), "{raw}");
        assert!(
            !f.holds("cas/c/x").await && !f.holds("cas/b/../c/x").await,
            "{raw}"
        );
    }
    // A doubly encoded `..` is a literal key inside the prefix, and goes nowhere else.
    let literal = resign_with_raw_path(&put_url("placeholder", 300), "cas/b/%252e%252e/c/x");
    assert_eq!(f.put(&literal, b"X").await.status, StatusCode::OK);
    assert!(f.holds("cas/b/%2e%2e/c/x").await);
    assert!(!f.holds("cas/c/x").await);
}

/// The URL of `url` with the object path `raw` as it is sent on the wire, signed again by the
/// presigning key over that path, so that only the check of the key can refuse it.
fn resign_with_raw_path(url: &str, raw: &str) -> String {
    use super::auth::{hmac, signing_key, string_to_sign_for};
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    let host = &rest[..at];
    let query = rest.split_once('?').unwrap().1;
    let query = query.rsplit_once("&X-Amz-Signature=").unwrap().0;
    let path = format!("/{BUCKET}/{raw}");
    let decoded = percent_encoding::percent_decode_str(&path)
        .decode_utf8()
        .unwrap()
        .into_owned();
    let canonical_path = super::auth::encode(&decoded, super::auth::PATH);
    let stamp = "20270115T080000Z";
    let canonical =
        format!("PUT\n{canonical_path}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let signing = signing_key(SECRET, &stamp[..8], "us-east-1");
    let to_sign = string_to_sign_for(stamp, (&stamp[..8], "us-east-1"), &canonical);
    let signature = hex::encode(hmac(&signing, to_sign.as_bytes()));
    format!("http://{host}{path}?{query}&X-Amz-Signature={signature}")
}

/// Asserts that a URL of the presigning key is good for one PUT and nothing else: a URL signed
/// for GET, HEAD or DELETE, a URL signed for PUT used with another method, a multipart call (start,
/// part, completion, abort) signed right, a put that asks to replace, and CreateBucket are all
/// refused, and nothing is stored, read or deleted.
#[tokio::test(start_paused = true)]
async fn a_presigning_key_never_reads_lists_deletes_or_uploads_in_parts() {
    let f = fixture().await;
    f.rig.put("cas/b/kept", b"CONTENT-OF-KEPT").await;
    for method in ["GET", "HEAD", "DELETE", "POST"] {
        let url = presign_url(
            method,
            &access("pack-presign"),
            &request("cas/b/kept", 300),
            &[],
        );
        let reply = f.send(method, &url.unwrap(), b"").await;
        assert_eq!(
            refused(&reply),
            (403, "AccessDenied".to_owned()),
            "{method}"
        );
        assert!(!reply.body.contains("CONTENT-OF-KEPT"), "{method}");
    }
    let put = put_url("cas/b/kept", 300);
    for method in ["GET", "HEAD", "DELETE"] {
        let reply = f.send(method, &put, b"").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method}");
    }
    let signed = |method: &str, extra: &[(&str, &str)]| {
        presign_url(
            method,
            &access("pack-presign"),
            &request("cas/b/big", 300),
            extra,
        )
        .unwrap()
    };
    let start = signed("POST", &[("uploads", "")]);
    assert_eq!(
        f.send("POST", &start, b"").await.status,
        StatusCode::FORBIDDEN
    );
    let part = signed("PUT", &[("partNumber", "1"), ("uploadId", "u")]);
    assert_eq!(f.put(&part, b"x").await.status, StatusCode::FORBIDDEN);
    let done = signed("POST", &[("uploadId", "u")]);
    assert_eq!(
        f.send("POST", &done, b"<x/>").await.status,
        StatusCode::FORBIDDEN
    );
    let abort = signed("DELETE", &[("uploadId", "u")]);
    assert_eq!(
        f.send("DELETE", &abort, b"").await.status,
        StatusCode::FORBIDDEN
    );
    for method in ["GET", "PUT", "HEAD"] {
        let bucket = presign_url(method, &access("pack-presign"), &request("", 300), &[]).unwrap();
        let reply = f.send(method, &bucket, b"").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method}");
    }
    let replace = request_for(
        "PUT",
        &put_url("cas/b/new", 300),
        &[("if-match", "\"abc\"")],
        b"x",
    );
    let reply = f.rig.send_raw(replace).await;
    assert_eq!(refused(&reply), (403, "AccessDenied".to_owned()));
    assert_eq!(f.rig.get("cas/b/kept", None).await.body, "CONTENT-OF-KEPT");
    assert!(!f.holds("cas/b/new").await && !f.holds("cas/b/big").await);
}

/// Asserts that a request signed in the header with the presigning key is refused, whatever it
/// asks for, and that the full key and a read-only key cannot sign a URL: the key of a URL is
/// never one with other rights.
#[tokio::test(start_paused = true)]
async fn the_presigning_key_signs_urls_only_and_other_keys_do_not() {
    let f = fixture().await;
    let pair = ("pack-presign", SECRET);
    for (method, uri) in [
        ("PUT", format!("/{BUCKET}/cas/b/x")),
        ("GET", format!("/{BUCKET}/cas/b/x")),
        ("GET", format!("/{BUCKET}?list-type=2&prefix=cas%2Fb%2F")),
        ("DELETE", format!("/{BUCKET}/cas/b/x")),
    ] {
        let signed = sign_as(&f.rig.config, pair, method, &uri, &[], b"x");
        let reply = f.rig.send_raw(signed).await;
        assert_eq!(
            refused(&reply),
            (403, "AccessDenied".to_owned()),
            "{method} {uri}"
        );
    }
    assert!(!f.holds("cas/b/x").await);
    for who in [
        AccessKey::new(KEY_ID, super::testing::SECRET),
        AccessKey::new("AKIDREADER", "reader-secret"),
    ] {
        let url = presign_url("PUT", &who, &request("cas/b/x", 300), &[]).unwrap();
        let reply = f.put(&url, b"x").await;
        assert_eq!(
            refused(&reply),
            (403, "InvalidAccessKeyId".to_owned()),
            "{}",
            who.id
        );
    }
    assert!(!f.holds("cas/b/x").await);
}

/// Asserts the size limit of the key: a body of the limit is stored; one byte more is
/// `EntityTooLarge` and stores nothing, whether the length is declared (refused before the body
/// is read) or only counted as the bytes arrive (a body with no declared length); and a body that
/// is cut part way stores nothing and leaves the key free for a complete put.
#[tokio::test(start_paused = true)]
async fn an_over_size_or_cut_body_leaves_nothing_visible() {
    let f = fixture().await;
    let ok = f.put(&put_url("cas/b/ok", 300), &[7_u8; 16]).await;
    assert_eq!(ok.status, StatusCode::OK);
    let over = [9_u8; 17];
    let counted = f.put(&put_url("cas/b/over", 300), &over).await;
    assert_eq!(refused(&counted), (413, "EntityTooLarge".to_owned()));
    let declared = request_for(
        "PUT",
        &put_url("cas/b/declared", 300),
        &[("content-length", "17")],
        &over,
    );
    assert_eq!(
        f.rig.send_raw(declared).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let lying = request_for(
        "PUT",
        &put_url("cas/b/lying", 300),
        &[("content-length", "4")],
        &over,
    );
    assert_eq!(
        f.rig.send_raw(lying).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let url = put_url("cas/b/cut", 300);
    let (parts, _) = request_for("PUT", &url, &[], b"").into_parts();
    let steps = vec![Step::Data(b"half"), Step::Fail];
    let cut = f
        .rig
        .send_raw(Request::from_parts(parts, Scripted::new(steps)))
        .await;
    assert!(cut.status.is_client_error(), "{}", cut.status);
    for key in ["cas/b/over", "cas/b/declared", "cas/b/lying", "cas/b/cut"] {
        assert!(!f.holds(key).await, "{key}");
        assert!(
            f.rig
                .hot_mem
                .list("cas/b/")
                .await
                .unwrap()
                .iter()
                .all(|o| o.key != key)
        );
    }
    assert_eq!(f.put(&url, b"whole").await.status, StatusCode::OK);
    assert_eq!(f.rig.get("cas/b/cut", None).await.body, "whole");
}

/// Asserts that the front's own limit holds when it is lower than the key's, and that the static
/// keys work as before beside a presigning key: the full key puts and gets anywhere, a read-only
/// key reads and cannot write.
#[tokio::test(start_paused = true)]
async fn static_keys_work_unchanged_beside_a_presigning_key() {
    let f = fixture_with(|config| config.max_request_bytes = 8).await;
    let over = f.put(&put_url("cas/b/nine", 300), &[1_u8; 9]).await;
    assert_eq!(over.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        f.put(&put_url("cas/b/eight", 300), &[1_u8; 8]).await.status,
        StatusCode::OK
    );
    assert_eq!(f.rig.put("cas/a", b"a").await.status, StatusCode::OK);
    assert_eq!(f.rig.get("cas/a", None).await.body, "a");
    let reader = ("AKIDREADER", "reader-secret");
    let read = f
        .rig
        .send_as(reader, "GET", &format!("/{BUCKET}/cas/a"), &[], b"")
        .await;
    assert_eq!(read.body, "a");
    let write = f
        .rig
        .send_as(reader, "PUT", &format!("/{BUCKET}/cas/b/z"), &[], b"z")
        .await;
    assert_eq!(write.status, StatusCode::FORBIDDEN);
    let delete = f
        .rig
        .send("DELETE", &format!("/{BUCKET}/cas/a"), &[], b"")
        .await;
    assert_eq!(delete.status, StatusCode::NO_CONTENT);
}

/// Asserts that two presigned PUTs of one key that run at once through one proxy have exactly one
/// winner: one answers 200, the other 412, and the stored bytes are the winner's, in one tier. It
/// catches a path in which both puts succeed, with different bytes in the two tiers.
#[tokio::test(start_paused = true)]
async fn two_concurrent_presigned_puts_of_one_key_have_one_winner() {
    let f = fixture().await;
    let (url_a, url_b) = (put_url("cas/b/race", 300), put_url("cas/b/race", 299));
    let (a, b) = tokio::join!(f.put(&url_a, b"AAAA"), f.put(&url_b, b"BBBB"));
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 412]);
    let winner: &[u8] = if a.status == StatusCode::OK {
        b"AAAA"
    } else {
        b"BBBB"
    };
    assert_eq!(f.rig.get("cas/b/race", None).await.body.as_bytes(), winner);
    let hot = f.rig.hot_mem.get("cas/b/race", None).await.ok();
    assert_eq!(hot.as_deref(), Some(winner));
    assert!(f.rig.cold_mem.get("cas/b/race", None).await.is_err());
}

/// Asserts the same for two proxies that share the hot and the cold tier, each behind a server of
/// its own: one 200, one 412, the winner's bytes, and the other tier holds nothing.
#[tokio::test(start_paused = true)]
async fn two_concurrent_presigned_puts_through_two_proxies_have_one_winner() {
    let f = fixture().await;
    let mut config = ProxyConfig::new("p2", MEBIBYTE, f.clock.clone());
    config.linger = Duration::from_millis(1);
    config.usage_refresh = Duration::ZERO;
    let second = Proxy::open(f.rig.hot.clone(), f.rig.cold.clone(), config)
        .await
        .unwrap();
    let other = Server::new(Arc::new(second), f.rig.config.clone());
    let url_a = put_url("cas/b/race", 300);
    let url_b = put_url("cas/b/race", 299);
    let first = f.put(&url_a, b"AAAA");
    let sent = other.handle(request_for("PUT", &url_b, &[], b"BBBB"));
    let (a, b) = tokio::join!(first, sent);
    let mut statuses = [a.status.as_u16(), b.status().as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 412]);
    let winner: &[u8] = if a.status == StatusCode::OK {
        b"AAAA"
    } else {
        b"BBBB"
    };
    let hot = f.rig.hot_mem.get("cas/b/race", None).await.ok();
    assert_eq!(hot.as_deref(), Some(winner));
    assert!(f.rig.cold_mem.get("cas/b/race", None).await.is_err());
}

/// Asserts that a presigned PUT never goes to the cold tier: when the usage says the hot tier is
/// full, and when the hot tier answers the put itself with a capacity error, the answer is 503
/// `SlowDown` with `Retry-After`, and neither tier holds the key. The same two cases for the full
/// key go to the cold tier as before.
#[tokio::test(start_paused = true)]
async fn a_presigned_put_when_the_hot_tier_is_full_is_503_and_stores_nothing() {
    let f = fixture().await;
    let usage = format!(
        r#"{{"hot_bytes":{},"copy_lag":0,"time":{NOW}}}"#,
        MEBIBYTE * 2
    );
    f.rig
        .hot_mem
        .put("_tiering/usage", Bytes::from(usage))
        .await
        .unwrap();
    let full = f.put(&put_url("cas/b/full", 300), b"x").await;
    assert_eq!(refused(&full), (503, "SlowDown".to_owned()));
    assert_eq!(full.header("retry-after"), "1");
    assert!(!f.holds("cas/b/full").await);
    let by_full_key = f.rig.put("cas/a/full", b"x").await;
    assert_eq!(by_full_key.status, StatusCode::OK);
    assert!(f.rig.cold_mem.get("cas/a/full", None).await.is_ok());
    f.rig.hot_mem.delete("_tiering/usage").await.unwrap();
    f.rig
        .hot
        .fail_next_on(Op::Put, "cas/b/mid", Error::Full("hot".to_owned()));
    let mid = f.put(&put_url("cas/b/mid", 300), b"x").await;
    assert_eq!(refused(&mid), (503, "SlowDown".to_owned()));
    assert!(!f.holds("cas/b/mid").await);
}

/// Asserts that a presigned PUT of a key that was tiered to the cold tier and is no longer in the
/// hot tier is 412 and leaves the cold object as it was.
#[tokio::test(start_paused = true)]
async fn a_presigned_put_of_a_key_in_the_cold_tier_only_is_412() {
    let f = fixture().await;
    f.rig
        .cold_mem
        .put("cas/b/old", Bytes::from_static(b"old"))
        .await
        .unwrap();
    let reply = f.put(&put_url("cas/b/old", 300), b"new").await;
    assert_eq!(refused(&reply), (412, "PreconditionFailed".to_owned()));
    assert_eq!(f.rig.cold_mem.get("cas/b/old", None).await.unwrap(), "old");
    assert!(f.rig.hot_mem.get("cas/b/old", None).await.is_err());
}

/// Asserts which query parameters the front takes: the six of query-string authentication on a
/// presigned request (they are read by the check of the signature and are not parameters of the
/// call), and no other that the front does not know, signed right or not; and that a request
/// signed in the header that carries one of the six (a presigned URL's signature in a
/// header-signed request, say) is `NotImplemented` and changes nothing. It catches a front that
/// answers 501 to every presigned URL, or one that lets any parameter through.
#[tokio::test(start_paused = true)]
async fn only_the_query_authentication_parameters_are_taken_on_a_presigned_request() {
    let f = fixture().await;
    let ok = f.put(&put_url("cas/b/ok", 300), b"x").await;
    assert_eq!(ok.status, StatusCode::OK);
    let extra = [("versionId", "1")];
    let url = presign_url(
        "PUT",
        &access("pack-presign"),
        &request("cas/b/v", 300),
        &extra,
    );
    let unknown = f.put(&url.unwrap(), b"x").await;
    assert_eq!(refused(&unknown), (501, "NotImplemented".to_owned()));
    assert!(!f.holds("cas/b/v").await);
    for param in [
        "X-Amz-Signature=00",
        "X-Amz-Date=20270115T080000Z",
        "X-Amz-Algorithm=x",
    ] {
        let uri = format!("/{BUCKET}/cas/a/h?{param}");
        let signed = sign_as(
            &f.rig.config,
            (KEY_ID, super::testing::SECRET),
            "PUT",
            &uri,
            &[],
            b"x",
        );
        let reply = f.rig.send_raw(signed).await;
        assert_eq!(
            refused(&reply),
            (501, "NotImplemented".to_owned()),
            "{param}"
        );
    }
    assert!(!f.holds("cas/a/h").await);
}

/// Asserts that a presigned PUT whose declared length is over the cap of its key is answered 413
/// before any byte of the body is read: the body of this request fails when it is read, so a
/// 400 (`IncompleteBody`) in its place would mean the body was read first. Nothing is stored,
/// and neither tier is asked for a put (the proxy registered itself in the hot tier when it
/// opened, so the count is taken before the request).
#[tokio::test(start_paused = true)]
async fn a_presigned_put_over_the_cap_is_413_before_the_body_is_read() {
    let f = fixture().await;
    let before = f.rig.hot.calls(Op::Put);
    let url = put_url("cas/b/big", 300);
    let (parts, _) = request_for("PUT", &url, &[("content-length", "17")], b"").into_parts();
    let failing = Scripted::new(vec![Step::Fail]);
    let reply = f.rig.send_raw(Request::from_parts(parts, failing)).await;
    assert_eq!(refused(&reply), (413, "EntityTooLarge".to_owned()));
    assert!(!f.holds("cas/b/big").await);
    assert_eq!(f.rig.hot.calls(Op::Put), before);
    assert_eq!(f.rig.cold.calls(Op::Put), 0);
}
