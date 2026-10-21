use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::body::HttpBody as _;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::routing::get;
use http_body_util::BodyExt as _;
use tower_service::Service;

use super::*;
use crate::proxy_auth::{ClientAddr, Identity};

/// A router with one route that answers a sized body, behind a limit of `per_caller`.
fn router(limit: &Arc<ReadLimit>) -> Router {
    Router::new()
        .route("/v1/thing/{id}", get(|| async { "twelve bytes" }))
        .layer(middleware::from_fn_with_state(Arc::clone(limit), limited))
}

/// One request of `path` from `peer` (none: a request with no connection info), with
/// `trust` in its extensions if given.
async fn call(router: &Router, path: &str, peer: Option<&str>, trust: Option<Trust>) -> Response {
    let mut request = Request::get(path).body(Body::empty()).expect("a request");
    if let Some(peer) = peer {
        let addr: SocketAddr = format!("{peer}:4000").parse().expect("an address");
        request.extensions_mut().insert(ConnectInfo(addr));
    }
    if let Some(trust) = trust {
        request.extensions_mut().insert(trust);
    }
    let mut router = router.clone();
    let Ok(response) = router.call(request).await;
    response
}

fn proxied(node: Option<&str>) -> Trust {
    Trust::Proxied(Identity {
        node_id: node.map(str::to_owned),
        ..Identity::default()
    })
}

async fn ok(router: &Router, peer: &str) -> Response {
    let response = call(router, "/v1/thing/x", Some(peer), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    response
}

/// Catches: no bound at all (mutant: the check `n >= per_caller` dropped, so the fifth
/// request is served); a bound shared by every caller (mutant: one key for all, so a second
/// caller is refused while the first is full: the starvation this guards); a slot that
/// does not return when its answer is dropped (mutant: the decrement dropped); a table
/// that keeps callers that have nothing open (mutant: the removal at zero dropped).
#[tokio::test]
async fn a_caller_holds_at_most_its_share_and_gets_the_slot_back() {
    let limit = ReadLimit::new(PER_CALLER);
    let router = router(&limit);
    let mut held = Vec::new();
    for _ in 0..PER_CALLER {
        held.push(ok(&router, "192.0.2.1").await);
    }
    assert_eq!(limit.callers(), 1);

    let refused = call(&router, "/v1/thing/x", Some("192.0.2.1"), None).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.headers()["retry-after"], "1");

    // Another caller is not affected by the first one being full.
    let other = ok(&router, "192.0.2.2").await;
    assert_eq!(limit.callers(), 2);
    drop(other);
    assert_eq!(limit.callers(), 1);

    // One answer dropped frees one slot, and only one.
    held.pop();
    let again = ok(&router, "192.0.2.1").await;
    let still = call(&router, "/v1/thing/x", Some("192.0.2.1"), None).await;
    assert_eq!(still.status(), StatusCode::TOO_MANY_REQUESTS);
    drop((again, held));
    assert_eq!(limit.callers(), 0, "an idle server remembers no caller");
}

/// Catches: a slot given back when the handler returns, not when the body is done (mutant:
/// the guard dropped before the body is built): a blob streams after its handler is
/// finished, so the bound would hold nothing; and a body that lost its size (a sized
/// answer sent chunked, which breaks a client that reads to the end of a plain answer).
#[tokio::test]
async fn the_slot_is_held_until_the_body_is_dropped_and_the_size_is_kept() {
    let limit = ReadLimit::new(1);
    let router = router(&limit);
    let response = ok(&router, "192.0.2.1").await;
    assert_eq!(response.body().size_hint().exact(), Some(12));
    assert!(!response.body().is_end_stream());
    let refused = call(&router, "/v1/thing/x", Some("192.0.2.1"), None).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    // Reading the body to its end is not the end of the answer; dropping it is.
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    assert_eq!(&bytes[..], b"twelve bytes");
    ok(&router, "192.0.2.1").await;
}

/// Catches: the caller taken from the address when the proxy named a node (mutant:
/// the `Node` key never used): an agent behind the proxy would share the browsers'
/// bound; a node's bound that follows its address (mutant: the address first); an
/// identity with no node taken for a caller of its own (mutant: `Peer` skipped when
/// the identity is proxied); and a request with no connection info refused or
/// panicking instead of keyed on the unspecified address.
#[tokio::test]
async fn a_node_the_proxy_named_has_a_bound_of_its_own() {
    let limit = ReadLimit::new(1);
    let router = router(&limit);
    // The proxy's own address, three callers: a node, the same node from another
    // address, and an anonymous browser.
    let node = call(
        &router,
        "/v1/thing/x",
        Some("127.0.0.1"),
        Some(proxied(Some("n1"))),
    )
    .await;
    assert_eq!(node.status(), StatusCode::OK);
    let again = call(
        &router,
        "/v1/thing/x",
        Some("192.0.2.9"),
        Some(proxied(Some("n1"))),
    )
    .await;
    assert_eq!(again.status(), StatusCode::TOO_MANY_REQUESTS, "same node");
    let browser = call(
        &router,
        "/v1/thing/x",
        Some("127.0.0.1"),
        Some(proxied(None)),
    )
    .await;
    assert_eq!(browser.status(), StatusCode::OK, "no node: its address");
    let second = call(
        &router,
        "/v1/thing/x",
        Some("127.0.0.1"),
        Some(Trust::Unverified),
    )
    .await;
    assert_eq!(
        second.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "same address"
    );
    let nobody = call(&router, "/v1/thing/x", None, None).await;
    assert_eq!(nobody.status(), StatusCode::OK);
    let nobody_again = call(&router, "/v1/thing/x", None, None).await;
    assert_eq!(nobody_again.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limit.callers(), 3);
    drop((node, again, browser, second, nobody, nobody_again));
    assert_eq!(limit.callers(), 0);
}

/// Catches: each bound of a request target dropped or moved (mutants: the path length,
/// the query length, the segment length and the control-character check each removed; a
/// bound off by one, `>` for `>=`, which the exact-size cases below would turn red), and
/// a refused request that still takes a slot (the table is empty after).
#[tokio::test]
async fn a_target_that_is_not_a_plain_path_is_refused_before_it_is_read() {
    let limit = ReadLimit::new(PER_CALLER);
    let router = router(&limit);
    let status = |path: String| {
        let router = router.clone();
        async move { call(&router, &path, Some("192.0.2.1"), None).await.status() }
    };
    let segment = "a".repeat(MAX_SEGMENT_BYTES);
    assert_eq!(status(format!("/v1/thing/{segment}")).await, StatusCode::OK);
    assert_eq!(
        status(format!("/v1/thing/{segment}a")).await,
        StatusCode::BAD_REQUEST
    );

    let query = "q".repeat(MAX_QUERY_BYTES - 2);
    assert_eq!(
        status(format!("/v1/thing/x?q={query}")).await,
        StatusCode::OK
    );
    assert_eq!(
        status(format!("/v1/thing/x?q={query}q")).await,
        StatusCode::URI_TOO_LONG
    );

    // Many short segments: the whole path is bounded too.
    let long = "/a".repeat(MAX_PATH_BYTES / 2 + 1);
    assert_eq!(status(long).await, StatusCode::URI_TOO_LONG);

    for hostile in ["%00", "%0a", "%1f", "%7f", "a%00b", "%0D%0A"] {
        assert_eq!(
            status(format!("/v1/thing/{hostile}")).await,
            StatusCode::BAD_REQUEST,
            "{hostile}"
        );
    }
    // Printable and non-ASCII bytes, encoded, are the route's to judge.
    for fine in ["%20", "%2F", "%C3%A9", "~", "a.b"] {
        assert_eq!(
            status(format!("/v1/thing/{fine}")).await,
            StatusCode::OK,
            "{fine}"
        );
    }
    assert_eq!(limit.callers(), 0);
}

/// Catches: a refusal that is not JSON with an `error` (the API's error shape): clients
/// of the read routes parse it.
#[tokio::test]
async fn a_refusal_is_the_apis_json_error() {
    let limit = ReadLimit::new(0);
    let router = router(&limit);
    let refused = call(&router, "/v1/thing/x", Some("192.0.2.1"), None).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.headers()["content-type"], "application/json");
    let body = refused
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|e| e.contains("too many"))
    );
}

/// One request from `peer` with `trust` and a `ClientAddr` of `client` in its extensions.
async fn call_as(router: &Router, peer: &str, trust: Trust, client: &str) -> Response {
    let mut request = Request::get("/v1/thing/x")
        .body(Body::empty())
        .expect("a request");
    let addr: SocketAddr = format!("{peer}:4000").parse().expect("an address");
    request.extensions_mut().insert(ConnectInfo(addr));
    request.extensions_mut().insert(trust);
    request
        .extensions_mut()
        .insert(ClientAddr(client.parse().expect("an address")));
    let mut router = router.clone();
    let Ok(response) = router.call(request).await;
    response
}

/// Catches: the client address the proxy reported keying a call the proxy did not vouch for
/// (mutant: the `Trust::Proxied` check dropped, so an `Unverified` or `Direct` call with a
/// `ClientAddr` in its extensions takes a bound of its own); and a proxied call's bound not
/// following the address (mutant: the address ignored, so two browsers share the proxy's
/// peer address).
#[tokio::test]
async fn a_client_address_keys_a_proxied_call_and_no_other() {
    let limit = ReadLimit::new(1);
    let router = router(&limit);
    // Not proxied: whatever the extensions hold, the key is the peer, so the second is refused.
    for trust in [Trust::Unverified, Trust::Direct] {
        let first = call_as(&router, "192.0.2.50", trust.clone(), "203.0.113.1").await;
        assert_eq!(first.status(), StatusCode::OK, "{trust:?}");
        let second = call_as(&router, "192.0.2.50", trust.clone(), "203.0.113.2").await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS, "{trust:?}");
        drop((first, second));
    }
    // Proxied: the address is the key, the peer (the proxy) is not.
    let one = call_as(&router, "127.0.0.1", proxied(None), "203.0.113.1").await;
    let two = call_as(&router, "127.0.0.1", proxied(None), "203.0.113.2").await;
    let again = call_as(&router, "127.0.0.1", proxied(None), "203.0.113.1").await;
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(two.status(), StatusCode::OK, "another browser");
    assert_eq!(
        again.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the same one"
    );
    drop((one, two, again));
}
