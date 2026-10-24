//! The security headers on the answers the proxy makes itself (the 503 of a request no member
//! takes, 404, 405, 400, 401, 426, the probes, the update API's reply) and their absence from a
//! forwarded child answer, which carries only what the child set, each header once.

use std::sync::atomic::Ordering;

use bytes::Bytes;
use http::HeaderMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_rustls::client::TlsStream;

use super::kit::{CAPS, OPERATOR, OPERATOR_TOKEN, Script, read_all, script, tagged};
use super::pki::{Pki, client_config};
use super::pool_kit::Rig;
use super::secure::{connect_tls, tls_rig};
use crate::auth::Buck2Auth;
use crate::reply::OWN_HEADERS;

/// The five headers and values the ruling names, written out here and not read from
/// `OWN_HEADERS`, so a change to the constant is a change to this test too.
const WANT: [(&str, &str); 5] = [
    (
        "content-security-policy",
        "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cache-control", "no-store"),
    ("cross-origin-resource-policy", "same-origin"),
];

fn exactly_once(headers: &HeaderMap, what: &str) {
    for (name, want) in WANT {
        let all: Vec<_> = headers.get_all(name).iter().collect();
        assert_eq!(all, [want], "{what}: {name}");
    }
}

/// The constant the proxy uses is the ruled set: not one more, not one fewer.
///
/// Kills: a header dropped from or added to the set (HSTS in particular) without the ruling.
#[test]
fn the_set_is_the_ruled_set() {
    assert_eq!(OWN_HEADERS, WANT);
}

/// Every answer the proxy makes itself over HTTP/2 carries each header exactly once: the
/// probes, `/metrics`, an unknown path, the UI's 404 (switched off) and 405, a 401, the 503 of a
/// request no member takes with its `Retry-After`, the update API's reply, and the
/// trailers-only gRPC answers (`UNIMPLEMENTED`, `UNAUTHENTICATED`, `UNAVAILABLE`).
///
/// Kills: a header missing on any of those paths (one constructor not secured); a header set
/// twice; the update reply's own headers replacing the set.
#[tokio::test(start_paused = true)]
async fn every_answer_the_proxy_makes_carries_each_header_once() {
    let rig = Rig::start(&[tagged("a")]);
    rig.core.ui.store(true, Ordering::Release);
    let mut client = rig.client().await;
    for (method, path, status) in [
        ("GET", "/healthz", 200),
        ("GET", "/metrics", 200),
        ("GET", "/no/such/path", 404),
        ("POST", "/v1/update/frontend", 503),
    ] {
        let reply = client.plain(method, path, b"").await;
        assert_eq!(reply.status, Some(status), "{method} {path}");
        exactly_once(&reply.headers, path);
    }
    let reply = client.unary("/no.such.Service/Method").await;
    assert_eq!(reply.grpc_status().as_deref(), Some("12"));
    exactly_once(&reply.headers, "grpc unknown");

    client.bearer = None;
    for (method, path, status) in [("POST", "/ui", 405), ("GET", OPERATOR, 401)] {
        let reply = client.plain(method, path, b"").await;
        assert_eq!(reply.status, Some(status), "{method} {path}");
        exactly_once(&reply.headers, path);
    }
    rig.core.ui.store(false, Ordering::Release);
    let reply = client.plain("GET", "/ui", b"").await;
    assert_eq!(reply.status, Some(404));
    exactly_once(&reply.headers, "ui off");

    client.bearer = Some(OPERATOR_TOKEN.to_owned());
    assert!(rig.members.set_draining("a", true));
    let reply = client.plain("GET", OPERATOR, b"").await;
    assert_eq!(reply.status, Some(503), "no member takes it");
    assert_eq!(reply.headers.get("retry-after").expect("header"), "1");
    exactly_once(&reply.headers, "unplaced 503");
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    exactly_once(&reply.headers, "unplaced grpc");
}

/// A refused gRPC call (no credential in `bearer` mode) is a trailers-only answer with the set.
///
/// Kills: `refused` building its gRPC answer outside the secured constructor.
#[tokio::test(start_paused = true)]
async fn a_refused_grpc_call_carries_the_set() {
    let rig = Rig::start_secure(Buck2Auth::Bearer, super::kit::Access::Plain(None));
    let mut client = rig.client().await;
    client.bearer = None;
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("16"));
    exactly_once(&reply.headers, "grpc refused");
}

/// A child that answers 200 with its own `content-security-policy` and `cache-control` (and
/// nothing else of the set).
fn opinionated_child() -> Script {
    script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "text/html")
            .header("content-security-policy", "default-src 'self'")
            .header("cache-control", "max-age=5")
            .body(())
            .expect("a response");
        if let Ok(mut send) = call.respond.send_response(response, false) {
            let _ = send.send_data(Bytes::from_static(b"<html>child</html>"), true);
        }
    })
}

/// A forwarded child answer keeps the child's values, each once, and gains none of the set:
/// the child owns its headers, the proxy owns only its own pages.
///
/// Kills: the set applied to forwarded answers (a doubled or replaced CSP; `referrer-policy`
/// or `cross-origin-resource-policy` appearing that the child never sent).
#[tokio::test(start_paused = true)]
async fn a_forwarded_answer_passes_through_unchanged() {
    let rig = Rig::start(&[opinionated_child()]);
    rig.core.ui.store(true, Ordering::Release);
    let mut client = rig.client().await;
    client.bearer = None;
    for path in ["/ui", "/ui/runs"] {
        let reply = client.plain("GET", path, b"").await;
        assert_eq!(reply.status, Some(200), "{path}");
        assert_eq!(reply.text(), "<html>child</html>");
        let csp: Vec<_> = reply
            .headers
            .get_all("content-security-policy")
            .iter()
            .collect();
        assert_eq!(csp, ["default-src 'self'"], "{path}");
        let cache: Vec<_> = reply.headers.get_all("cache-control").iter().collect();
        assert_eq!(cache, ["max-age=5"], "{path}");
        for name in [
            "x-content-type-options",
            "referrer-policy",
            "cross-origin-resource-policy",
        ] {
            assert!(reply.headers.get(name).is_none(), "{path}: {name} added");
        }
    }
    client.bearer = Some(OPERATOR_TOKEN.to_owned());
    let reply = client.plain("GET", OPERATOR, b"").await;
    assert_eq!(reply.status, Some(200));
    let csp: Vec<_> = reply
        .headers
        .get_all("content-security-policy")
        .iter()
        .collect();
    assert_eq!(csp, ["default-src 'self'"]);
    assert!(reply.headers.get("referrer-policy").is_none());
}

async fn exchange(mut tls: TlsStream<DuplexStream>, request: &str) -> String {
    tls.write_all(request.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    // A read error after the data (no close_notify) still leaves what arrived in `out`.
    let _ = tls.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn h1(rig: &Rig, pki: &Pki, request: &str) -> String {
    let tls = connect_tls(rig, client_config(&pki.ca_pem, None, &["http/1.1"]))
        .await
        .expect("the handshake");
    exchange(tls, request).await
}

fn request(line: &str, extra: &str) -> String {
    format!(
        "{line} HTTP/1.1\r\nHost: frontend.test\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// The header lines of a raw HTTP/1.1 answer, names lowercased.
fn head_lines(response: &str) -> Vec<String> {
    response
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .lines()
        .skip(1)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Over HTTP/1.1 the proxy's own answers (400, 405, 401, 426, the 503 of a request no member
/// takes, a 404) carry each header once on the wire, and a forwarded answer carries none of them.
///
/// Kills: the h1 path building an answer the HTTP/2 path does not (`bad_request`,
/// `upgrade_required`); a set that is only applied to HTTP/2; the set on a forwarded answer.
#[tokio::test(start_paused = true)]
async fn http11_answers_carry_the_set_once_and_forwarded_ones_do_not() {
    let (rig, pki, _) = tls_rig("own-headers-h1", Buck2Auth::None).await;
    rig.core.ui.store(true, Ordering::Release);
    let auth = format!("Authorization: Bearer {OPERATOR_TOKEN}\r\n");
    let grpc = "Content-Type: application/grpc\r\n";
    let own = [
        ("400", request("GET http://elsewhere.test/v1/status", "")),
        ("405", request("POST /ui", "")),
        ("401", request(&format!("GET {OPERATOR}"), "")),
        ("426", request(&format!("POST {CAPS}"), grpc)),
        ("404", request("GET /no/such/path", &auth)),
    ];
    for (status, raw) in &own {
        let response = h1(&rig, &pki, raw).await;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{response}"
        );
        let lines = head_lines(&response);
        for (name, want) in WANT {
            let line = format!("{name}: {want}");
            let found = lines
                .iter()
                .filter(|l| l.starts_with(&format!("{name}:")))
                .count();
            assert_eq!(found, 1, "{status}: {name} in {lines:?}");
            assert!(
                lines.contains(&line.to_ascii_lowercase()),
                "{status}: {line}"
            );
        }
    }
    let forwarded = h1(&rig, &pki, &request(&format!("GET {OPERATOR}"), &auth)).await;
    assert!(forwarded.starts_with("HTTP/1.1 200"), "{forwarded}");
    let lines = head_lines(&forwarded);
    for (name, _) in WANT {
        assert!(
            !lines.iter().any(|l| l.starts_with(&format!("{name}:"))),
            "forwarded answer gained {name}: {lines:?}"
        );
    }

    assert!(rig.members.set_draining("a", true));
    let unplaced = h1(&rig, &pki, &request(&format!("GET {OPERATOR}"), &auth)).await;
    assert!(unplaced.starts_with("HTTP/1.1 503"), "{unplaced}");
    let lines = head_lines(&unplaced);
    for (name, want) in WANT {
        let found = lines
            .iter()
            .filter(|l| l.starts_with(&format!("{name}:")))
            .count();
        assert_eq!(found, 1, "unplaced 503: {name} in {lines:?}");
        assert!(
            lines.contains(&format!("{name}: {want}").to_ascii_lowercase()),
            "unplaced 503: {name}"
        );
    }
    assert!(lines.contains(&"retry-after: 1".to_owned()), "{lines:?}");
}
