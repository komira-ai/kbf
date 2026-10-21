//! The address a browser connected to the proxy from keys the per-caller read bound
//! (`kbf_server::read_limit`), but only when the proxy says it (`x-kbf-proxy-auth`): the
//! real gate (`gate_requests`) in front of the real limiter, over the headers the proxy
//! sends. Behind the proxy every connection is from loopback and an open `/ui` request names
//! no node, so without this every browser shares one bound.

#![cfg(unix)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderName, Request, Response, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use kbf_server::proxy_auth::{
    CLIENT_ADDR, NODE_ID, PROXY_AUTH, ProxySecret, ROLE, client_addr, gate_requests,
};
use kbf_server::read_limit::{PER_CALLER, ReadLimit, limited};
use tower_service::Service;

const SECRET: [u8; 32] = [0x5a; 32];
const LOOPBACK: &str = "127.0.0.1:4000";

fn secret() -> Arc<ProxySecret> {
    let dir =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-client-addr");
    let path = dir.join("secret");
    std::fs::write(&path, SECRET).expect("write the secret");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    Arc::new(ProxySecret::from_file(&path).expect("a usable secret"))
}

/// The operator API's shape: the gate outermost, the limiter inside it, one read route.
fn api(secret: Option<Arc<ProxySecret>>) -> Router {
    Router::new()
        .route("/ui", get(|| async { "page" }))
        .layer(from_fn_with_state(ReadLimit::new(PER_CALLER), limited))
        .layer(from_fn_with_state(secret, gate_requests))
}

/// One `GET /ui` from `peer` with `headers`.
async fn get_ui(router: &Router, peer: &str, headers: &[(&str, &str)]) -> Response<Body> {
    let mut request = Request::get("/ui").body(Body::empty()).expect("a request");
    for (name, value) in headers {
        request.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).expect("a name"),
            value.parse().expect("a value"),
        );
    }
    let peer: SocketAddr = peer.parse().expect("an address");
    request.extensions_mut().insert(ConnectInfo(peer));
    let mut router = router.clone();
    let Ok(response) = router.call(request).await;
    response
}

/// What the proxy sends for an open browser at `client`: the secret and the address.
fn browser(client: &str) -> [(&'static str, String); 2] {
    [
        (PROXY_AUTH, hex::encode(SECRET)),
        (CLIENT_ADDR, client.to_owned()),
    ]
}

async fn as_browser(router: &Router, client: &str) -> Response<Body> {
    let headers = browser(client);
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    get_ui(router, LOOPBACK, &pairs).await
}

/// The name is a contract with `kbf-proxy`, which defines it too.
#[test]
fn the_header_name_is_the_proxys() {
    assert_eq!(CLIENT_ADDR, "x-kbf-client-addr");
}

/// Two browsers behind the proxy, from different addresses, each get their own
/// [`PER_CALLER`] open reads; the fifth of each is `429`; a request with no address (an older
/// proxy) and a node that the proxy named are other callers again.
///
/// Mutants: `caller_of` not reading `ClientAddr` (every browser is loopback: the second
/// browser's first read is `429`: the starvation this fixes); the address read but the node
/// id not first (the node is keyed by its address, so it shares with the browser below).
#[tokio::test]
async fn two_browsers_behind_the_proxy_each_get_their_own_bound() {
    let router = api(Some(secret()));
    let mut held = Vec::new();
    for client in ["192.0.2.10", "198.51.100.4"] {
        for n in 0..PER_CALLER {
            let response = as_browser(&router, client).await;
            assert_eq!(response.status(), StatusCode::OK, "{client} read {n}");
            held.push(response);
        }
        let fifth = as_browser(&router, client).await;
        assert_eq!(fifth.status(), StatusCode::TOO_MANY_REQUESTS, "{client}");
    }
    // No address from the proxy: keyed by the peer, loopback, as before this header.
    let only = [(PROXY_AUTH, hex::encode(SECRET))];
    let pairs: Vec<(&str, &str)> = only.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let old = get_ui(&router, LOOPBACK, &pairs).await;
    assert_eq!(old.status(), StatusCode::OK, "an older proxy's call");
    // A node the proxy named has its own key, whatever address came with it.
    let node = [
        (PROXY_AUTH, hex::encode(SECRET)),
        (CLIENT_ADDR, "192.0.2.10".to_owned()),
        (NODE_ID, "node-1.farm".to_owned()),
        (ROLE, "daemon".to_owned()),
    ];
    let pairs: Vec<(&str, &str)> = node.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let named = get_ui(&router, LOOPBACK, &pairs).await;
    assert_eq!(named.status(), StatusCode::OK, "a named node");
    drop(held);
}

/// A direct request that carries the header does not change its key: with a proxy
/// configured but no (or the wrong) secret, and with no proxy at all, a caller that varies
/// `x-kbf-client-addr` is still one caller (its peer), so it cannot take a fresh bound by
/// writing one.
///
/// Mutants: both checks gone at once, `admit` recording the address whatever the trust and
/// `caller_of` not checking `Trust::Proxied` (the forged values are separate callers and no
/// fifth read is refused). Each check alone is hidden by the other here, so each has its own
/// test: `admit` below, `caller_of` in `read_limit/tests.rs`.
#[tokio::test]
async fn a_forged_header_on_a_direct_request_does_not_change_the_key() {
    // (what, a proxy configured, the secret the caller presents)
    let cases = [
        ("no secret presented", true, None),
        ("a wrong secret", true, Some("00")),
        ("no proxy configured", false, None),
    ];
    for (what, configured, presented) in cases {
        let router = api(configured.then(secret));
        let mut held = Vec::new();
        for n in 0..=PER_CALLER {
            let forged = format!("203.0.113.{n}");
            let mut headers = vec![(CLIENT_ADDR, forged.as_str())];
            if let Some(value) = presented {
                headers.push((PROXY_AUTH, value));
            }
            let response = get_ui(&router, LOOPBACK, &headers).await;
            if n < PER_CALLER {
                assert_eq!(response.status(), StatusCode::OK, "{what} read {n}");
            } else {
                assert_eq!(
                    response.status(),
                    StatusCode::TOO_MANY_REQUESTS,
                    "{what}: a fresh address is not a fresh bound"
                );
            }
            held.push(response);
        }
    }
}

/// The address is one canonical IP: an IPv4-mapped IPv6 spelling is the same browser as its
/// IPv4 form (not a second bound for it), and a value that is not an address is refused
/// before the read, like any malformed identity header.
///
/// Mutants: `to_canonical` dropped (the mapped spelling is a fifth-read escape); a value
/// that does not parse taken as no address instead of refused (`403`).
#[tokio::test]
async fn the_address_is_canonical_and_a_bad_one_is_refused() {
    let router = api(Some(secret()));
    let mut held = Vec::new();
    for _ in 0..PER_CALLER {
        held.push(as_browser(&router, "192.0.2.10").await);
    }
    let mapped = as_browser(&router, "::ffff:192.0.2.10").await;
    assert_eq!(mapped.status(), StatusCode::TOO_MANY_REQUESTS);
    for bad in [
        "not-an-address",
        "192.0.2.10:4000",
        "",
        "192.0.2.10,192.0.2.11",
    ] {
        let refused = as_browser(&router, bad).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN, "{bad:?}");
    }
    // Twice is not one value.
    let headers = [
        (PROXY_AUTH, hex::encode(SECRET)),
        (CLIENT_ADDR, "192.0.2.10".to_owned()),
        (CLIENT_ADDR, "192.0.2.11".to_owned()),
    ];
    let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let twice = get_ui(&router, LOOPBACK, &pairs).await;
    assert_eq!(twice.status(), StatusCode::FORBIDDEN);
    drop(held);
}

/// What a handler can see of the address: only a proxied call has one in its extensions, and
/// the header itself is gone for every call.
///
/// Mutants: `admit` recording the address whatever the trust (the limiter's own
/// `Trust::Proxied` check would hide it from the key, so only the extension shows it);
/// the header left in place for a handler.
#[tokio::test]
async fn only_a_proxied_call_has_a_client_address_and_no_call_keeps_the_header() {
    async fn seen(request: axum::extract::Request) -> String {
        format!(
            "{:?} header={}",
            request
                .extensions()
                .get::<kbf_server::proxy_auth::ClientAddr>(),
            request.headers().contains_key(CLIENT_ADDR)
        )
    }
    let body_of = |response: Response<Body>| async move {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 10)
            .await
            .expect("a body");
        String::from_utf8(bytes.to_vec()).expect("text")
    };
    let route = |secret: Option<Arc<ProxySecret>>| {
        Router::new()
            .route("/ui", get(seen))
            .layer(from_fn_with_state(secret, gate_requests))
    };
    let proxied = route(Some(secret()));
    let answer = body_of(as_browser(&proxied, "192.0.2.10").await).await;
    assert_eq!(answer, "Some(ClientAddr(192.0.2.10)) header=false");
    let forged = [(CLIENT_ADDR, "203.0.113.9")];
    let answer = body_of(get_ui(&proxied, LOOPBACK, &forged).await).await;
    assert_eq!(answer, "None header=false", "a secret-less call");
    let direct = route(None);
    let answer = body_of(get_ui(&direct, LOOPBACK, &forged).await).await;
    assert_eq!(answer, "None header=false", "no proxy configured");
}

/// `client_addr` on its own: absent is `None`, one address is that address, canonical.
#[test]
fn client_addr_reads_one_address_or_none() {
    let mut h = axum::http::HeaderMap::new();
    assert_eq!(client_addr(&h), Ok(None));
    h.insert(CLIENT_ADDR, "2001:db8::1".parse().expect("a value"));
    let got = client_addr(&h).expect("one address").expect("some");
    assert_eq!(
        got.0,
        "2001:db8::1".parse::<std::net::IpAddr>().expect("ip")
    );
}
