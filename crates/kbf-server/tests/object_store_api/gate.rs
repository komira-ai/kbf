//! The write gate on every route, a GET included, the feature switch, and the headers.

use kbf_types::object_store_api::ErrorBody;

use super::harness::{BIND_BODY, Farm, SENTINEL, TOKEN, conn_body, loopback, send};

/// Every route with a body that would succeed if the gate let it through.
fn routes() -> Vec<(&'static str, &'static str, String)> {
    let conn = conn_body(SENTINEL);
    vec![
        ("GET", "/v1/object-stores", String::new()),
        ("GET", "/v1/object-stores/audit", String::new()),
        ("GET", "/v1/object-stores/connections", String::new()),
        ("GET", "/v1/object-stores/connections/cold", String::new()),
        ("PUT", "/v1/object-stores/connections/cold", conn),
        (
            "DELETE",
            "/v1/object-stores/connections/cold",
            String::new(),
        ),
        (
            "POST",
            "/v1/object-stores/connections/cold:verify",
            "{}".to_owned(),
        ),
        (
            "PUT",
            "/v1/object-stores/bindings/cold",
            BIND_BODY.to_owned(),
        ),
        ("DELETE", "/v1/object-stores/bindings/cold", String::new()),
    ]
}

/// A refusal: JSON with a closed code, no cache, no secret, and nothing changed.
fn assert_refused(farm: &Farm, reply: &super::harness::Reply, status: u16, code: &str, what: &str) {
    assert_eq!(reply.status, status, "{what}: {}", reply.body);
    let body: ErrorBody = reply.json();
    assert_eq!(body.code, code, "{what}");
    assert!(!body.error.is_empty(), "{what}");
    assert_eq!(reply.header("cache-control"), Some("no-store"), "{what}");
    assert_eq!(
        reply.header("content-type"),
        Some("application/json"),
        "{what}"
    );
    assert!(!reply.body.contains(SENTINEL), "{what}: {}", reply.body);
    assert_eq!(
        farm.generation(),
        0,
        "{what}: the gate refused, so nothing is written"
    );
    assert!(farm.files().is_empty(), "{what}");
    assert!(
        farm.probe.calls().is_empty(),
        "{what}: no store is contacted"
    );
}

// Catches a route that skips a gate step, above all a GET that answers without the token
// (these routes list the farm's store and its key id), an Origin header that is accepted
// (a browser page must not act with the operator's token), a bearer that is wrong or missing,
// a content type other than JSON (curl -d sends a form), a peer that is not this host, and
// a server with writes off. Each refusal answers its own status with a closed code and
// `Cache-Control: no-store`, writes nothing and contacts no store.
#[tokio::test]
async fn every_route_passes_the_whole_gate_a_get_included() {
    let bearer = format!("Bearer {TOKEN}");
    let json = ("content-type", "application/json");
    for (method, path, body) in routes() {
        let what = |why: &str| format!("{method} {path} {why}");
        let farm = Farm::new();

        let no_token = farm.with(method, path, &[json], &body).await;
        assert_refused(
            &farm,
            &no_token,
            401,
            "unauthorized",
            &what("without a bearer"),
        );
        assert_eq!(no_token.header("www-authenticate"), Some("Bearer"));
        let wrong = farm
            .with(
                method,
                path,
                &[
                    json,
                    (
                        "authorization",
                        "Bearer not-the-token-0123456789abcdef012345",
                    ),
                ],
                &body,
            )
            .await;
        assert_refused(
            &farm,
            &wrong,
            401,
            "unauthorized",
            &what("with a wrong bearer"),
        );
        let scheme = farm
            .with(method, path, &[json, ("authorization", TOKEN)], &body)
            .await;
        assert_refused(
            &farm,
            &scheme,
            401,
            "unauthorized",
            &what("without the Bearer scheme"),
        );

        let origin = farm
            .with(
                method,
                path,
                &[
                    json,
                    ("authorization", &bearer),
                    ("origin", "http://evil.example"),
                ],
                &body,
            )
            .await;
        assert_refused(&farm, &origin, 403, "forbidden", &what("from a page"));

        let form = farm
            .with(
                method,
                path,
                &[
                    ("content-type", "application/x-www-form-urlencoded"),
                    ("authorization", &bearer),
                ],
                &body,
            )
            .await;
        assert_refused(
            &farm,
            &form,
            415,
            "unsupported_media_type",
            &what("as a form"),
        );
        let bare = farm
            .with(method, path, &[("authorization", &bearer)], &body)
            .await;
        assert_refused(
            &farm,
            &bare,
            415,
            "unsupported_media_type",
            &what("without a content type"),
        );

        let remote = send(
            &farm.router,
            "192.0.2.7:40000".parse().expect("addr"),
            method,
            path,
            &[json, ("authorization", &bearer)],
            body.as_bytes(),
        )
        .await;
        assert_refused(&farm, &remote, 403, "forbidden", &what("from another host"));

        let off = Farm::with_token(None);
        let writes_off = off
            .with(method, path, &[json, ("authorization", &bearer)], &body)
            .await;
        assert_refused(
            &off,
            &writes_off,
            403,
            "forbidden",
            &what("with writes off"),
        );
    }
}

// Catches a feature switch that is checked before the gate (an anonymous caller could learn
// whether the feature is on), or one that lets a route work with no registry. A server
// without the feature answers a caller who passed the gate with 404 and `not_enabled` on
// every route, and still refuses one who did not.
#[tokio::test]
async fn a_frontend_without_the_feature_answers_404_to_a_caller_past_the_gate() {
    use axum::Router;
    let router: Router = super::harness::gated(
        kbf_server::object_store::api::routes(Some(super::harness::token_file()), None),
        None,
    );
    let bearer = format!("Bearer {TOKEN}");
    for (method, path, body) in routes() {
        let anonymous = send(
            &router,
            loopback(),
            method,
            path,
            &[("content-type", "application/json")],
            body.as_bytes(),
        )
        .await;
        assert_eq!(anonymous.status, 401, "{method} {path}: {}", anonymous.body);
        let past = send(
            &router,
            loopback(),
            method,
            path,
            &[
                ("content-type", "application/json"),
                ("authorization", &bearer),
            ],
            body.as_bytes(),
        )
        .await;
        assert_eq!(past.status, 404, "{method} {path}: {}", past.body);
        assert_eq!(past.code(), "not_enabled");
        assert_eq!(past.header("cache-control"), Some("no-store"));
    }
}

// Catches an answer that a cache may keep (a 405 and a 404 included: the layer covers
// the router, not just the handlers) and a verb the routes do not offer answering
// something other than 405.
#[tokio::test]
async fn every_answer_is_no_store_and_a_wrong_method_is_405() {
    let farm = Farm::new();
    let wrong = farm
        .call("PATCH", "/v1/object-stores/connections/cold", "{}")
        .await;
    assert_eq!(wrong.status, 405);
    assert_eq!(wrong.header("cache-control"), Some("no-store"));
    let wrong = farm.call("POST", "/v1/object-stores/audit", "{}").await;
    assert_eq!(wrong.status, 405);
    let wrong = farm.call("DELETE", "/v1/object-stores", "").await;
    assert_eq!(wrong.status, 405);
    let wrong = farm
        .call("POST", "/v1/object-stores/bindings/cold", "{}")
        .await;
    assert_eq!(wrong.status, 405);
}

// Catches a handler that takes the peer from the connection alone (or builds a `Caller`
// of its own) and so never sees the proxy gate's `Trust`: behind a proxy, a write that
// reaches the loopback port without the proxy's secret, however it dresses itself in
// `x-kbf-*` identity headers and the right bearer token, must be refused 403 on every
// route, and a call from another host is refused before any handler. With the secret and
// the token the same calls pass: the proxy's word is not the token, and the token is
// still required (401 without it).
//
// Mutants seen red: each handler's `Caller` replaced by `Caller::direct(peer.addr)` (the
// forged-header calls were admitted); `operator_gate`'s `Unverified` refusal skipped.
#[tokio::test]
async fn behind_a_proxy_only_a_call_with_its_secret_and_the_token_passes_the_gate() {
    let bearer = format!("Bearer {TOKEN}");
    let secret = hex::encode(super::harness::PROXY_SECRET);
    let json = ("content-type", "application/json");
    let forged = [("x-kbf-node-id", "node-a"), ("x-kbf-role", "daemon")];
    for (method, path, body) in routes() {
        let what = |why: &str| format!("{method} {path} {why}");
        let farm = Farm::behind_proxy();

        let mut no_secret = vec![json, ("authorization", bearer.as_str())];
        no_secret.extend(forged);
        let reply = farm.with(method, path, &no_secret, &body).await;
        assert_refused(
            &farm,
            &reply,
            403,
            "forbidden",
            &what("with forged identity headers and no secret"),
        );

        let not_the_secret = "00".repeat(32);
        let mut wrong = no_secret.clone();
        wrong.push(("x-kbf-proxy-auth", not_the_secret.as_str()));
        let reply = farm.with(method, path, &wrong, &body).await;
        assert_refused(
            &farm,
            &reply,
            403,
            "forbidden",
            &what("with a wrong secret"),
        );

        let mut proxied = no_secret.clone();
        proxied.push(("x-kbf-proxy-auth", &secret));
        let remote = send(
            &farm.router,
            "192.0.2.7:40000".parse().expect("addr"),
            method,
            path,
            &proxied,
            body.as_bytes(),
        )
        .await;
        assert_eq!(remote.status, 403, "{}", what("from another host"));
        assert_eq!(farm.generation(), 0, "{}", what("from another host"));
        assert!(farm.files().is_empty(), "{}", what("from another host"));
        assert!(
            farm.probe.calls().is_empty(),
            "{}",
            what("from another host")
        );

        let without_token = [json, ("x-kbf-proxy-auth", secret.as_str())];
        let reply = farm.with(method, path, &without_token, &body).await;
        assert_refused(
            &farm,
            &reply,
            401,
            "unauthorized",
            &what("with the secret and no token"),
        );

        let reply = farm.with(method, path, &proxied, &body).await;
        assert!(
            reply.status != 401 && reply.status != 403,
            "{}: {}",
            what("with the secret and the token"),
            reply.body
        );
    }
}
