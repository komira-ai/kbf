//! The read-only web UI through the proxy: `/ui` is open (no certificate, no token), takes only
//! GET and HEAD, can be switched off, passes the server's security headers through untouched,
//! and its being open never opens anything else. The last is the one that matters: every
//! operator, update, metrics, worker and REAPI path still refuses a caller with no credential.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::kit::{CAPS, Rig, Script, read_all, script};
use super::pki::client_config;
use super::secure::{connect_tls, tls_rig};
use crate::auth::Buck2Auth;

const CSP: &str = "default-src 'none'; style-src 'self'; script-src 'none'";

type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// A child that records each request's method and target and answers 200 `text/html` with
/// the security headers the server sets (the proxy must carry them back as they are).
fn page(seen: &Seen) -> Script {
    let seen = Arc::clone(seen);
    script(move |mut call| {
        let seen = Arc::clone(&seen);
        async move {
            read_all(&mut call.body).await;
            let target = call
                .uri
                .path_and_query()
                .map_or_else(String::new, |p| p.as_str().to_owned());
            let method = call.method.to_string();
            let head = call.method == http::Method::HEAD;
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((method, target));
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "text/html; charset=utf-8")
                .header("content-security-policy", CSP)
                .header("x-content-type-options", "nosniff")
                .header("cache-control", "no-store")
                .body(())
                .expect("a response");
            if head {
                let _ = call.respond.send_response(response, true);
            } else if let Ok(mut send) = call.respond.send_response(response, false) {
                let _ = send.send_data(Bytes::from_static(b"<html>page</html>"), true);
            }
        }
    })
}

/// A plain rig whose children are [`page`]s; `/ui` is served only if `ui` (the `ui` key of the
/// `[proxy]` table).
async fn rig(seen: &Seen, ui: bool) -> Rig {
    let rig = Rig::start(page(seen), page(&Seen::default())).await;
    rig.core.ui.store(ui, Ordering::Release);
    rig
}

fn recorded(seen: &Seen) -> Vec<(String, String)> {
    seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// A caller with no credential at all reads the UI: the index, a nested page with a query,
/// the stylesheet, and HEAD. The child sees the method and the target whole, and the
/// server's security headers come back exactly once with the server's values.
///
/// Kills: `/ui` behind the operator token; the query or path changed on the way; the proxy
/// adding or rewriting a header the server owns (a doubled or conflicting CSP).
#[tokio::test(start_paused = true)]
async fn the_ui_is_open_and_reaches_the_child_whole() {
    let seen = Seen::default();
    let rig = rig(&seen, true).await;
    let mut client = rig.client().await;
    client.bearer = None;
    let targets = [
        "/ui",
        "/ui/",
        "/ui/invocations/abc?page_token=x%20y&limit=5",
        "/ui/static/ui.css",
    ];
    for target in targets {
        let reply = client.plain("GET", target, b"").await;
        assert_eq!(reply.status, Some(200), "{target}");
        assert_eq!(reply.text(), "<html>page</html>", "{target}");
        for (name, want) in [
            ("content-security-policy", CSP),
            ("x-content-type-options", "nosniff"),
            ("cache-control", "no-store"),
        ] {
            let all: Vec<_> = reply.headers.get_all(name).iter().collect();
            assert_eq!(all, [want], "{target}: {name}");
        }
    }
    let head = client.plain("HEAD", "/ui/runs", b"").await;
    assert_eq!(head.status, Some(200));
    assert!(head.body.is_empty(), "a HEAD answer has no body");
    let mut want: Vec<_> = targets.map(|t| ("GET".to_owned(), t.to_owned())).to_vec();
    want.push(("HEAD".to_owned(), "/ui/runs".to_owned()));
    assert_eq!(recorded(&seen), want);
}

/// Every method but GET and HEAD is a 405 at the proxy that names the two allowed, and the
/// child is never asked.
///
/// Kills: a write forwarded to the child; a 405 without `Allow`; the check on `/ui` but not
/// on the pages under it.
#[tokio::test(start_paused = true)]
async fn a_write_to_the_ui_is_refused_at_the_proxy() {
    let seen = Seen::default();
    let rig = rig(&seen, true).await;
    let mut client = rig.client().await;
    client.bearer = None;
    for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        for target in ["/ui", "/ui/runs/r1"] {
            let reply = client.plain(method, target, b"").await;
            assert_eq!(reply.status, Some(405), "{method} {target}");
            let allow = reply.headers.get("allow").expect("Allow");
            assert_eq!(allow, "GET, HEAD", "{method} {target}");
        }
    }
    let with_body = client.plain("POST", "/ui/runs/r1", b"x=1").await;
    assert_eq!(with_body.status, Some(405));
    assert_eq!(rig.a.state.dispatches(), 0);
    assert!(recorded(&seen).is_empty());
}

/// `ui = false` makes `/ui` a 404 like any path outside the table, and does not touch the
/// operator API beside it.
///
/// Kills: the `ui` key ignored; `ui = false` that also closes `/v1`.
#[tokio::test(start_paused = true)]
async fn ui_off_is_a_404_and_leaves_the_operator_api_alone() {
    let seen = Seen::default();
    let rig = rig(&seen, false).await;
    let mut client = rig.client().await;
    client.bearer = None;
    for target in ["/ui", "/ui/", "/ui/runs"] {
        for method in ["GET", "POST"] {
            let reply = client.plain(method, target, b"").await;
            assert_eq!(reply.status, Some(404), "{method} {target}");
        }
    }
    assert!(recorded(&seen).is_empty());
    client.bearer = Some(super::kit::OPERATOR_TOKEN.to_owned());
    let reply = client.plain("GET", "/v1/status", b"").await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(recorded(&seen).len(), 1);
}

/// A path under `/ui` that is not plain segments is a 404 and never reaches the child: the UI
/// is open, so a raw `..` or an encoded slash must not be the way to another route's path.
///
/// Kills: `/ui/../v1/nodes` forwarded under the open class.
#[tokio::test(start_paused = true)]
async fn a_ui_path_cannot_walk_out_of_the_ui() {
    let seen = Seen::default();
    let rig = rig(&seen, true).await;
    let mut client = rig.client().await;
    client.bearer = None;
    for target in [
        "/ui/../v1/nodes",
        "/ui/./runs",
        "/ui/%2e%2e/v1/nodes",
        "/ui/%2E%2E/v1/nodes",
        "/ui/a%2fb",
        "/ui/a%5Cb",
        "/ui//x",
        "/ui/a/../../v1/nodes",
        "/uix",
        "/ui.css",
    ] {
        let reply = client.plain("GET", target, b"").await;
        assert_eq!(reply.status, Some(404), "{target}");
    }
    assert!(recorded(&seen).is_empty());
}

/// What a call with no credential came to: `Some(401)`-class refusal as `true`.
async fn refused(client: &mut super::kit::Client, method: &str, path: &str) -> bool {
    if crate::route::route_of(path).grpc() {
        let reply = client.unary(path).await;
        return reply.grpc_status().as_deref() == Some("16");
    }
    client.plain(method, path, b"").await.status == Some(401)
}

/// The UI being open opens nothing else: with no certificate and no token, every operator
/// route (GET and POST), the update API, `/metrics`, and the worker and (in `mtls` and
/// `bearer` mode) REAPI paths are refused as before, whatever `buck2_auth` is, and none of
/// them reaches a child. Look-alikes of `/ui` under `/v1` are operator routes, not UI ones.
///
/// Kills: the UI class leaking to a `/v1` route, `/metrics` or the update API; a prefix match
/// that takes `/v1/ui` or `/uix` for the UI.
#[tokio::test(start_paused = true)]
async fn the_ui_being_open_opens_no_other_route() {
    let operator = [
        "/v1/status",
        "/v1/nodes",
        "/v1/nodes/n1:drain",
        "/v1/ui",
        "/v1/ui/runs",
        "/v1/uix",
        "/v1/blobs/abc",
        "/v1/invocations/i1",
        "/v1/update/desired",
        "/v1/update/frontend",
        "/v1/update/frontend/abort",
        "/metrics",
    ];
    let worker = [
        "/kbf.worker.v1.Worker/Session",
        "/kbf.map.v1.Membership/Join",
        "/kbf.enroll.v1.ColdStore/GetKeys",
    ];
    for mode in [Buck2Auth::None, Buck2Auth::Mtls, Buck2Auth::Bearer] {
        let rig = Rig::start_secure(mode, super::kit::Access::Plain(None)).await;
        let mut client = rig.client().await;
        client.bearer = None;
        for path in operator.into_iter().chain(worker) {
            for method in ["GET", "POST"] {
                assert!(
                    refused(&mut client, method, path).await,
                    "{mode:?} {method} {path}"
                );
            }
        }
        if mode != Buck2Auth::None {
            assert!(refused(&mut client, "POST", CAPS).await, "{mode:?} {CAPS}");
        }
        assert_eq!(rig.a.state.dispatches(), 0, "{mode:?}: a child was reached");
    }
}

/// Over TLS and HTTP/1.1, as a browser comes: no client certificate, no token, GET is served,
/// POST is a 405 and neither reaches the child unless it is the GET, while an operator path
/// beside it is still a 401.
///
/// Kills: `/ui` demanding a client certificate; HTTP/1.1 refusing the UI (a 426 for it);
/// the method check skipped on HTTP/1.1.
#[tokio::test(start_paused = true)]
async fn a_browser_with_no_certificate_reads_the_ui_over_http11() {
    let (rig, pki, _) = tls_rig("ui-h1", Buck2Auth::Mtls).await;
    let request = |line: &str| {
        format!(
            "{line} HTTP/1.1\r\nHost: frontend.test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    };
    let mut answers = Vec::new();
    for line in ["GET /ui", "HEAD /ui/runs", "POST /ui", "GET /v1/status"] {
        let tls = connect_tls(&rig, client_config(&pki.ca_pem, None, &["http/1.1"]))
            .await
            .expect("the handshake");
        answers.push(exchange(tls, &request(line)).await);
    }
    let status = |i: usize| answers[i].lines().next().unwrap_or_default().to_owned();
    assert!(status(0).starts_with("HTTP/1.1 200"), "{}", answers[0]);
    assert!(status(1).starts_with("HTTP/1.1 200"), "{}", answers[1]);
    assert!(status(2).starts_with("HTTP/1.1 405"), "{}", answers[2]);
    assert!(status(3).starts_with("HTTP/1.1 401"), "{}", answers[3]);
    assert_eq!(rig.a.state.dispatches(), 2, "the GET and the HEAD only");
}

async fn exchange(
    mut tls: tokio_rustls::client::TlsStream<tokio::io::DuplexStream>,
    request: &str,
) -> String {
    tls.write_all(request.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    // A read error after the data (no close_notify) still leaves what arrived in `out`.
    let _ = tls.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}
