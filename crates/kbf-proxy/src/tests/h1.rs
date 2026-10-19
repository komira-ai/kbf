//! HTTP/1.1 on the TLS listener: served for `/v1/*`, the update API and the probes, refused
//! for gRPC, bounded in its head, and made to look like HTTP/2 to the child.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_rustls::client::TlsStream;

use super::kit::{CAPS, OPERATOR, OPERATOR_TOKEN, Rig};
use super::pki::{Pki, client_config};
use super::secure::{connect_tls, tls_rig};
use crate::auth::Buck2Auth;

/// One request over `tls`, and everything the server sent back until it closed.
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

fn get(path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: frontend.test\r\n{extra}Connection: close\r\n\r\n")
}

fn status_of(response: &str) -> &str {
    response.lines().next().unwrap_or_default()
}

/// The probes and an operator path are served over HTTP/1.1; the child sees the request as
/// an HTTP/2 one (no `connection`, no `host`), the secret, and none of the client's `x-kbf-*`.
///
/// Kills: an h1 that does not serve; connection-level headers passed to an h2 child; the
/// strip skipped on h1.
#[tokio::test(start_paused = true)]
async fn http11_serves_probes_and_operator_paths() {
    let (rig, pki, _) = tls_rig("h1-serves", Buck2Auth::None).await;
    let health = h1(&rig, &pki, &get("/healthz", "")).await;
    assert!(status_of(&health).starts_with("HTTP/1.1 200"), "{health}");

    let auth = format!("Authorization: Bearer {OPERATOR_TOKEN}\r\nx-kbf-node-id: forged\r\n");
    let served = h1(&rig, &pki, &get("/v1/status?x=1", &auth)).await;
    assert!(status_of(&served).starts_with("HTTP/1.1 200"), "{served}");
    assert!(served.contains("\r\na\r\n"), "the child's answer: {served}");
    let seen = rig.a.state.headers_of(OPERATOR);
    assert_eq!(seen.len(), 1);
    assert!(seen[0].get("connection").is_none() && seen[0].get("host").is_none());
    assert!(seen[0].get("x-kbf-node-id").is_none());
    assert!(seen[0].get("x-kbf-proxy-auth").is_some());

    let refused = h1(&rig, &pki, &get(OPERATOR, "")).await;
    assert!(status_of(&refused).starts_with("HTTP/1.1 401"), "{refused}");
    assert_eq!(rig.a.state.dispatches(), 1);
}

/// A client that offers no ALPN at all is an HTTP/1.1 client.
///
/// Kills: no ALPN treated as h2.
#[tokio::test(start_paused = true)]
async fn no_alpn_is_http11() {
    let (rig, pki, _) = tls_rig("h1-no-alpn", Buck2Auth::None).await;
    let tls = connect_tls(&rig, client_config(&pki.ca_pem, None, &[]))
        .await
        .expect("the handshake");
    let response = exchange(tls, &get("/healthz", "")).await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 200"),
        "{response}"
    );
}

/// A gRPC path on HTTP/1.1 is `426 Upgrade Required` naming `h2`, and never reaches a child.
///
/// Kills: h1 serving a gRPC path.
#[tokio::test(start_paused = true)]
async fn a_grpc_path_on_http11_is_426() {
    let (rig, pki, _) = tls_rig("h1-426", Buck2Auth::None).await;
    for path in [
        CAPS,
        "/kbf.worker.v1.Worker/Session",
        "/kbf.enroll.v1.Enroll/Enroll",
        "/google.bytestream.ByteStream/Write",
    ] {
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: frontend.test\r\nContent-Type: application/grpc\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let response = h1(&rig, &pki, &request).await;
        assert!(
            status_of(&response).starts_with("HTTP/1.1 426"),
            "{path}: {response}"
        );
        assert!(
            response.to_ascii_lowercase().contains("upgrade: h2"),
            "{response}"
        );
    }
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A path outside the table is a 404 on HTTP/1.1, as on HTTP/2.
///
/// Kills: an unknown path forwarded on h1.
#[tokio::test(start_paused = true)]
async fn an_unknown_path_on_http11_is_404() {
    let (rig, pki, _) = tls_rig("h1-404", Buck2Auth::None).await;
    let response = h1(&rig, &pki, &get("/not/in/the/table", "")).await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 404"),
        "{response}"
    );
    let response = h1(
        &rig,
        &pki,
        "OPTIONS * HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 404"),
        "{response}"
    );
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// `CONNECT`, an absolute-form target and an `Upgrade` header are `400`.
///
/// Kills: the proxy acting as a forward proxy or switching protocols.
#[tokio::test(start_paused = true)]
async fn only_origin_form_requests_without_upgrade_are_served() {
    let (rig, pki, _) = tls_rig("h1-400", Buck2Auth::None).await;
    let requests = [
        "CONNECT frontend.test:443 HTTP/1.1\r\nHost: frontend.test:443\r\nConnection: close\r\n\r\n"
            .to_owned(),
        get("http://elsewhere.test/v1/status", ""),
        get("/healthz", "Upgrade: websocket\r\n"),
    ];
    for request in &requests {
        let response = h1(&rig, &pki, request).await;
        assert!(
            status_of(&response).starts_with("HTTP/1.1 400"),
            "{request}: {response}"
        );
    }
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A head over 16 KiB, or with over 100 headers, is refused and the connection closes.
///
/// Kills: no head limit.
#[tokio::test(start_paused = true)]
async fn the_request_head_is_bounded() {
    let (rig, pki, _) = tls_rig("h1-limits", Buck2Auth::None).await;
    let big = format!("x-padding: {}\r\n", "a".repeat(20_000));
    let response = h1(&rig, &pki, &get("/healthz", &big)).await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 431"),
        "{}",
        status_of(&response)
    );

    let many: String = (0..crate::h1::MAX_HEADERS + 1)
        .map(|n| format!("x-n{n}: 1\r\n"))
        .collect();
    let response = h1(&rig, &pki, &get("/healthz", &many)).await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 431"),
        "{}",
        status_of(&response)
    );

    let ok: String = (0..crate::h1::MAX_HEADERS - 5)
        .map(|n| format!("x-n{n}: 1\r\n"))
        .collect();
    let response = h1(&rig, &pki, &get("/healthz", &ok)).await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 200"),
        "{}",
        status_of(&response)
    );
}

/// A client that sends its head too slowly is closed at the pre-stream bound.
///
/// Kills: no head timeout.
#[tokio::test(start_paused = true)]
async fn a_slow_head_is_closed() {
    let (rig, pki, _) = tls_rig("h1-slow", Buck2Auth::None).await;
    let mut tls = connect_tls(&rig, client_config(&pki.ca_pem, None, &["http/1.1"]))
        .await
        .expect("the handshake");
    tls.write_all(b"GET /healthz HT")
        .await
        .expect("a partial head");
    let mut rest = Vec::new();
    let done = tokio::time::timeout(Duration::from_secs(30), tls.read_to_end(&mut rest)).await;
    assert!(done.is_ok(), "the server kept a half-sent head for 30 s");
}

/// A request for a backend that the gate holds waits, and at the bound is answered `503`
/// with `Retry-After`; the probes are not held.
///
/// Kills: a held h1 request that never answers; a probe held.
#[tokio::test(start_paused = true)]
async fn a_held_http11_request_is_503_at_the_bound() {
    let (rig, pki, _) = tls_rig("h1-hold", Buck2Auth::None).await;
    let _guard = rig.core.slots.hold(Duration::from_secs(20)).expect("hold");
    let health = h1(&rig, &pki, &get("/healthz", "")).await;
    assert!(status_of(&health).starts_with("HTTP/1.1 200"), "{health}");

    let auth = format!("Authorization: Bearer {OPERATOR_TOKEN}\r\n");
    let response = h1(&rig, &pki, &get(OPERATOR, &auth)).await;
    assert!(
        status_of(&response).starts_with("HTTP/1.1 503"),
        "{response}"
    );
    assert!(
        response.to_ascii_lowercase().contains("retry-after: 1"),
        "{response}"
    );
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A shutdown closes an idle HTTP/1.1 connection and lets the server end.
///
/// Kills: an HTTP/1.1 connection that outlives the shutdown.
#[tokio::test(start_paused = true)]
async fn a_shutdown_closes_an_idle_http11_connection() {
    let (rig, pki, _) = tls_rig("h1-shutdown", Buck2Auth::None).await;
    let mut tls = connect_tls(&rig, client_config(&pki.ca_pem, None, &["http/1.1"]))
        .await
        .expect("the handshake");
    tls.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .expect("write");
    let mut buf = [0_u8; 512];
    let n = tls.read(&mut buf).await.expect("an answer");
    assert!(buf[..n].starts_with(b"HTTP/1.1 200"));
    rig.shutdown();
    rig.serving.await.expect("the serve task ends");
    let mut rest = Vec::new();
    let _ = tls.read_to_end(&mut rest).await;
}
