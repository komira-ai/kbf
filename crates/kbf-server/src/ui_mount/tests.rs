use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::routing::get;
use tower_service::Service;

use super::*;
use crate::proxy_auth::Caller;

/// A router with the routes the sources below ask.
fn api() -> Router {
    Router::new()
        .route("/v1/thing", get(|| async { "{\"n\": 1}" }))
        .route("/v1/big", get(|| async { "x".repeat(1000) }))
        .route(
            "/v1/who",
            get(|peer: Caller| async move { format!("{} {:?}", peer.addr, peer.trust) }),
        )
}

async fn send(router: &Router, method: Method, path: &str, peer: &str) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("a request");
    let addr: SocketAddr = format!("{peer}:4000").parse().expect("an address");
    request.extensions_mut().insert(ConnectInfo(addr));
    let mut router = router.clone();
    let Ok(response) = router.call(request).await;
    response
}

async fn text(response: Response) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("text")
}

/// Catches: a source that does not return the status, the body and the headers of the
/// route it asked (mutants: the status fixed at 200, the headers dropped), one that
/// reads a route with the caller's authority (the request must carry the lowest trust and
/// the local address), an answer past its bound read whole (mutant: no `Limited`), and a
/// target that is not a request refused as a read error, not a panic.
#[tokio::test]
async fn the_source_asks_a_route_as_the_lowest_trust_and_bounds_the_answer() {
    let source = InProcess::bounded(api(), 100);
    let thing = source.get("/v1/thing").await.expect("an answer");
    assert_eq!(thing.status, 200);
    assert_eq!(thing.body, b"{\"n\": 1}");
    assert!(
        thing
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.starts_with("text/plain")),
        "{:?}",
        thing.headers
    );

    let gone = source.get("/v1/nothing").await.expect("an answer");
    assert_eq!(gone.status, 404);

    let who = source.get("/v1/who").await.expect("an answer");
    assert_eq!(
        String::from_utf8(who.body).expect("text"),
        "127.0.0.1:0 Unverified"
    );

    let big = source.get("/v1/big").await.expect_err("past the bound");
    assert!(big.0.contains("/v1/big"), "{big}");
    assert!(source.get("/v1/thing with spaces").await.is_err());
    let roomy = InProcess::new(api());
    assert_eq!(roomy.get("/v1/big").await.expect("answer").body.len(), 1000);
}

/// Catches: a method other than `GET` and `HEAD` served, or answered without the security
/// headers (mutants: `post`, `any` for `get`; the `secure` layer dropped or placed inside
/// the limit); `HEAD` refused or sent with a body.
#[tokio::test]
async fn the_pages_serve_get_and_head_and_refuse_every_other_method_with_the_headers() {
    let pages = pages(InProcess::new(api()));
    let get = send(&pages, Method::GET, "/ui", "10.0.0.1").await;
    assert_eq!(get.status(), StatusCode::OK);
    assert!(
        get.headers()["content-type"]
            .to_str()
            .expect("text")
            .starts_with("text/html")
    );
    let page = text(get).await;
    assert!(page.contains("<title>Overview - kbf</title>"), "{page}");

    let slash = send(&pages, Method::GET, "/ui/", "10.0.0.1").await;
    assert_eq!(slash.status(), StatusCode::OK);

    let head = send(&pages, Method::HEAD, "/ui", "10.0.0.1").await;
    assert_eq!(head.status(), StatusCode::OK);
    assert!(text(head).await.is_empty());

    for method in [
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::OPTIONS,
        Method::TRACE,
    ] {
        for path in [
            "/ui",
            "/ui/",
            "/ui/builds",
            "/ui/static/kbf.css",
            "/ui/builds/x",
        ] {
            let refused = send(&pages, method.clone(), path, "10.0.0.1").await;
            assert_eq!(
                refused.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path}"
            );
            assert_headers(&refused, &format!("{method} {path}"));
        }
    }
}

fn assert_headers(response: &Response, what: &str) {
    let h = response.headers();
    assert_eq!(
        h["content-security-policy"],
        kbf_ui::CONTENT_SECURITY_POLICY,
        "{what}"
    );
    assert_eq!(h["x-content-type-options"], "nosniff", "{what}");
    assert_eq!(h["referrer-policy"], "no-referrer", "{what}");
    assert_eq!(h["cache-control"], "no-store", "{what}");
}

/// Catches: the security headers on a page but not on the stylesheet, a page the UI does
/// not know, a refused target (`414`, `400`) or a `429` (mutants: the `secure` layer
/// inside the limiter, so the limiter's own answers have none; set only on `200`); a
/// header a handler set not overridden (the handler sets its own policy here).
#[tokio::test]
async fn every_answer_under_ui_carries_the_security_headers() {
    let pages = pages(InProcess::new(api()));
    let long = format!(
        "/ui/builds/{}",
        "a".repeat(crate::read_limit::MAX_SEGMENT_BYTES + 1)
    );
    for (path, status) in [
        ("/ui", 200),
        ("/ui/static/kbf.css", 200),
        ("/ui/builds", 200),
        ("/ui/nothing-here", 404),
        ("/ui/builds/%00", 400),
        (long.as_str(), 400),
    ] {
        let response = send(&pages, Method::GET, path, "10.0.0.1").await;
        assert_eq!(response.status().as_u16(), status, "{path}");
        assert_headers(&response, path);
    }
    let too_long = format!("/ui/{}", "a/".repeat(crate::read_limit::MAX_PATH_BYTES));
    let response = send(&pages, Method::GET, &too_long, "10.0.0.1").await;
    assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
    assert_headers(&response, "a path too long");
}

/// Catches: the limiter missing from `/ui` (mutant: the layer removed), and one caller's
/// open pages refusing another's: a held answer is a `429` for its caller and nobody else.
#[tokio::test]
async fn ui_reads_are_bounded_per_caller_with_the_headers_on_the_429() {
    let pages = pages(InProcess::new(api()));
    let mut held = Vec::new();
    for _ in 0..PER_CALLER {
        held.push(send(&pages, Method::GET, "/ui", "10.0.0.1").await);
    }
    let refused = send(&pages, Method::GET, "/ui", "10.0.0.1").await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.headers()["retry-after"], "1");
    assert_headers(&refused, "429");
    let other = send(&pages, Method::GET, "/ui", "10.0.0.2").await;
    assert_eq!(other.status(), StatusCode::OK);
    drop(held);
    let after = send(&pages, Method::GET, "/ui", "10.0.0.1").await;
    assert_eq!(after.status(), StatusCode::OK);
}
