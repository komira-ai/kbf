//! The routes, through the actor, with the real bearer check.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};

use super::auth::{BearerAuth, PeerKey, RateLimit, Sha256Hasher};
use super::core::CrashPoint;
use super::fakes::{COMMIT_C, FakeClock, FakeForwarder, FakeTokens, Rig, TOKEN, api, idle, staged};
use super::model::OldChild;
use super::router::{MAX_BODY, Router};
use super::traits::{Compat, InstallerState, InstallerStatus};
use crate::proxy::model::Phase;
use crate::testkit::{A, B};

struct Harness {
    rig: Rig,
    router: Router,
    forwarded: Arc<Mutex<Vec<(String, String)>>>,
}

fn harness(name: &str, rate: RateLimit) -> Harness {
    let rig = Rig::new(name);
    let forwarder = FakeForwarder::default();
    let forwarded = forwarder.seen.clone();
    let router = router_over(&rig, forwarder, rate);
    Harness {
        rig,
        router,
        forwarded,
    }
}

fn router_over(rig: &Rig, forwarder: FakeForwarder, rate: RateLimit) -> Router {
    let auth = BearerAuth::new(
        Box::new(FakeTokens(Mutex::new(Some(TOKEN.to_owned())))),
        Box::new(Sha256Hasher),
        Arc::new(FakeClock(AtomicU64::new(0))),
        rate,
    );
    Router::new(Arc::new(auth), api(rig), Arc::new(forwarder))
}

fn quick() -> RateLimit {
    RateLimit {
        burst: 10_000,
        per_second: 10_000,
    }
}

fn call(method: Method, path: &str, body: Value, token: Option<&str>) -> Request<Bytes> {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Bytes::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn send(h: &Harness, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = h.router.call(call(method, path, body, Some(TOKEN))).await;
    (resp.status(), parse(&resp))
}

fn parse(resp: &Response<Bytes>) -> Value {
    serde_json::from_slice(resp.body()).unwrap_or(Value::Null)
}

fn post_body(commit: &str, request_id: &str) -> Value {
    json!({"commit": commit, "request_id": request_id, "nodes": true, "rollback": false})
}

const U: &str = "/v1/update/frontend";

#[tokio::test]
async fn post_answers_202_with_the_update_id_and_the_same_one_for_a_retry() {
    let h = harness("u8a-r-post", quick());
    let (status, first) = send(&h, Method::POST, U, post_body(B, "r1")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(first["phase"], "accepted");
    let (status, again) = send(&h, Method::POST, U, post_body(B, "r1")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(first["update_id"], again["update_id"]);
    assert_eq!(h.rig.orders.written.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn post_defaults_are_nodes_true_and_rollback_false() {
    let h = harness("u8a-r-defaults", quick());
    let (status, _) = send(
        &h,
        Method::POST,
        U,
        json!({"commit": B, "request_id": "r1"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    h.rig.set_status(staged(B));
    send(&h, Method::GET, U, Value::Null).await;
    let spec = h.rig.sup.0.lock().unwrap().starts[0].clone();
    assert!(spec.nodes && !spec.rollback);
}

#[tokio::test]
async fn bodies_that_are_not_requests_are_400() {
    let h = harness("u8a-r-400", quick());
    let bodies = [
        json!({"commit": "abc", "request_id": "r"}),
        json!({"commit": B.to_uppercase(), "request_id": "r"}),
        json!({"commit": B}),
        json!({"commit": B, "request_id": "r", "extra": 1}),
        json!({"commit": B, "request_id": "has space"}),
        json!("a string"),
    ];
    for body in bodies {
        let (status, v) = send(&h, Method::POST, U, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(v["error"].is_string());
    }
    assert!(h.rig.orders.written.lock().unwrap().is_empty());
    let resp = h
        .router
        .call(call(Method::POST, U, Value::Null, Some(TOKEN)))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "null is not a request"
    );
    let mut raw = Request::builder().method(Method::POST).uri(U);
    raw = raw.header("authorization", format!("Bearer {TOKEN}"));
    let resp = h
        .router
        .call(raw.body(Bytes::from_static(b"{")).unwrap())
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_oversized_body_is_413() {
    let h = harness("u8a-r-413", quick());
    let mut req = call(Method::POST, U, Value::Null, Some(TOKEN));
    *req.body_mut() = Bytes::from(vec![b' '; MAX_BODY + 1]);
    assert_eq!(
        h.router.call(req).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
}

#[tokio::test]
async fn the_409s_and_the_422_reach_the_client_with_their_codes() {
    let h = harness("u8a-r-409", quick());
    let (s, v) = send(&h, Method::POST, U, post_body(A, "r0")).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("same_commit"))
    );
    *h.rig.gate.before.lock().unwrap() = Compat::Incompatible {
        rule: "R1".to_owned(),
    };
    let (s, v) = send(&h, Method::POST, U, post_body(B, "r1")).await;
    assert_eq!(
        (s, v["error"].as_str(), v["rule"].as_str()),
        (StatusCode::CONFLICT, Some("incompatible"), Some("R1"))
    );
    *h.rig.gate.before.lock().unwrap() = Compat::Incomplete {
        reason: "no tarball".to_owned(),
    };
    let (s, v) = send(&h, Method::POST, U, post_body(B, "r1")).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("incomplete_release"))
    );
    *h.rig.gate.before.lock().unwrap() = Compat::Compatible;
    send(&h, Method::POST, U, post_body(B, "r2")).await;
    let (s, v) = send(&h, Method::POST, U, post_body(COMMIT_C, "r3")).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("update_open"))
    );
}

#[tokio::test]
async fn get_reports_the_status_shape() {
    let h = harness("u8a-r-get", quick());
    let (s, v) = send(&h, Method::GET, U, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["update_id"], Value::Null);
    assert_eq!(v["restart_pending"], false);
    h.rig.sup.set(
        Phase::Baking,
        Some(OldChild {
            open_streams: 4,
            oldest_stream_age_s: 90,
            open_executions: 2,
            queued_operations: 1,
        }),
        0,
    );
    let (_, v) = send(&h, Method::GET, U, Value::Null).await;
    assert_eq!(
        v,
        json!({
            "update_id": "upd-9", "from": A, "to": B, "phase": "baking",
            "since": 1_800_000_000u64, "active_slot": "b", "restart_pending": false,
            "rollback_allowed": true, "rollback_blocked_reason": null,
            "old": {"open_streams": 4, "oldest_stream_age_s": 90, "open_executions": 2, "queued_operations": 1},
            "new": {"open_streams": 3}, "failure": null
        })
    );
}

#[tokio::test]
async fn a_refused_staging_shows_as_failed_through_get() {
    let h = harness("u8a-r-refused", quick());
    send(&h, Method::POST, U, post_body(B, "r1")).await;
    h.rig.set_status(InstallerStatus {
        state: InstallerState::Refused,
        refusal: "attestation: not attested".to_owned(),
        acked_order_id: 101,
        ..idle()
    });
    let (s, v) = send(&h, Method::GET, U, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["phase"], "failed");
    assert_eq!(
        v["failure"],
        json!({"step": "verify", "reason": "attestation"})
    );
    assert_eq!(h.rig.sup.starts(), 0);
}

#[tokio::test]
async fn abort_rollback_and_force_retire_have_their_routes() {
    let h = harness("u8a-r-verbs", quick());
    h.rig.sup.set(Phase::Preflight, None, 0);
    let (s, v) = send(&h, Method::POST, "/v1/update/frontend/abort", Value::Null).await;
    assert_eq!(
        (s, v["phase"].as_str()),
        (StatusCode::OK, Some("rolled_back"))
    );
    let (s, v) = send(&h, Method::POST, "/v1/update/frontend/abort", json!({})).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("no_update_open"))
    );

    let old = OldChild {
        open_streams: 6,
        ..OldChild::default()
    };
    h.rig.sup.set(Phase::Baking, Some(old), 1);
    let (s, v) = send(
        &h,
        Method::POST,
        "/v1/update/frontend/rollback",
        Value::Null,
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str(), v["reason"].as_str()),
        (
            StatusCode::CONFLICT,
            Some("rollback_refused"),
            Some("nodesupdated")
        )
    );
    h.rig.sup.set(Phase::Baking, Some(old), 0);
    let (s, v) = send(
        &h,
        Method::POST,
        "/v1/update/frontend/rollback",
        Value::Null,
    )
    .await;
    assert_eq!(
        (s, v["phase"].as_str()),
        (StatusCode::OK, Some("rolled_back"))
    );

    h.rig.sup.set(Phase::Draining, Some(old), 2);
    let (s, v) = send(
        &h,
        Method::POST,
        "/v1/update/frontend/force-retire",
        json!({}),
    )
    .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("reason_required"))
    );
    let (s, v) = send(
        &h,
        Method::POST,
        "/v1/update/frontend/force-retire",
        json!({"reason": "hung"}),
    )
    .await;
    assert_eq!(
        (s, v["dropped_streams"].as_u64()),
        (StatusCode::OK, Some(6))
    );
    let (s, _) = send(
        &h,
        Method::POST,
        "/v1/update/frontend/force-retire",
        json!({"reason": "x", "y": 1}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn wrong_methods_and_unknown_paths() {
    let h = harness("u8a-r-methods", quick());
    for (m, p) in [
        (Method::DELETE, U),
        (Method::PUT, U),
        (Method::GET, "/v1/update/frontend/abort"),
        (Method::GET, "/v1/update/frontend/rollback"),
        (Method::GET, "/v1/update/frontend/force-retire"),
    ] {
        let resp = h
            .router
            .call(call(m.clone(), p, Value::Null, Some(TOKEN)))
            .await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{m} {p}");
        assert!(resp.headers().contains_key("allow"));
    }
    // Unknown paths are 404 without a credential; known ones are not.
    for p in ["/v1/update/frontend/nope", "/v1/other", "/"] {
        let resp = h.router.call(call(Method::GET, p, Value::Null, None)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{p}");
    }
    let resp = h
        .router
        .call(call(Method::POST, "/v1/update", Value::Null, Some(TOKEN)))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "only GET /v1/update is forwarded"
    );
}

#[tokio::test]
async fn auth_guards_every_route_and_nothing_runs_without_it() {
    let h = harness("u8a-r-auth", quick());
    for (m, p) in [
        (Method::POST, U),
        (Method::GET, U),
        (Method::POST, "/v1/update/frontend/abort"),
        (Method::POST, "/v1/update/frontend/rollback"),
        (Method::POST, "/v1/update/frontend/force-retire"),
        (Method::POST, "/v1/update/desired"),
        (Method::POST, "/v1/update/pause"),
        (Method::POST, "/v1/update/resume"),
        (Method::GET, "/v1/update"),
    ] {
        let none = h
            .router
            .call(call(m.clone(), p, post_body(B, "r"), None))
            .await;
        assert_eq!(none.status(), StatusCode::UNAUTHORIZED, "{m} {p}");
        assert_eq!(none.headers()["www-authenticate"], "Bearer");
        let wrong = h
            .router
            .call(call(m.clone(), p, post_body(B, "r"), Some("wrong")))
            .await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED, "{m} {p}");
        assert_eq!(parse(&none)["error"], parse(&wrong)["error"]);
    }
    let mut twice = call(Method::GET, U, Value::Null, Some(TOKEN));
    twice
        .headers_mut()
        .append("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let resp = h.router.call(twice).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(parse(&resp)["error"], "malformed_credentials");
    let q = h
        .router
        .call(call(
            Method::POST,
            &format!("{U}?token={TOKEN}"),
            post_body(B, "r"),
            Some(TOKEN),
        ))
        .await;
    assert_eq!(q.status(), StatusCode::BAD_REQUEST);
    assert_eq!(parse(&q)["error"], "token_in_query");
    assert!(h.rig.orders.written.lock().unwrap().is_empty());
    assert!(h.forwarded.lock().unwrap().is_empty());
    assert_eq!(h.rig.sup.starts(), 0);
}

#[tokio::test]
async fn the_rate_limit_answers_429_with_retry_after() {
    let h = harness(
        "u8a-r-429",
        RateLimit {
            burst: 2,
            per_second: 1,
        },
    );
    for _ in 0..2 {
        let (s, _) = send(&h, Method::GET, U, Value::Null).await;
        assert_eq!(s, StatusCode::OK);
    }
    let resp = h
        .router
        .call(call(Method::GET, U, Value::Null, Some(TOKEN)))
        .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.headers()["retry-after"], "1");
    let mut other = call(Method::GET, U, Value::Null, Some(TOKEN));
    other
        .extensions_mut()
        .insert(PeerKey("elsewhere".to_owned()));
    assert_eq!(h.router.call(other).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_unconfigured_token_is_503() {
    let rig = Rig::new("u8a-r-notoken");
    let auth = BearerAuth::new(
        Box::new(FakeTokens(Mutex::new(None))),
        Box::new(Sha256Hasher),
        Arc::new(FakeClock(AtomicU64::new(0))),
        quick(),
    );
    let router = Router::new(
        Arc::new(auth),
        api(&rig),
        Arc::new(FakeForwarder::default()),
    );
    let resp = router
        .call(call(Method::GET, U, Value::Null, Some(TOKEN)))
        .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn the_operator_routes_are_forwarded_after_auth() {
    let h = harness("u8a-r-forward", quick());
    for (m, p) in [
        (Method::POST, "/v1/update/desired"),
        (Method::GET, "/v1/update/desired"),
        (Method::POST, "/v1/update/resume"),
        (Method::GET, "/v1/update"),
    ] {
        let resp = h.router.call(call(m, p, Value::Null, Some(TOKEN))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(parse(&resp), json!({"forwarded": true}));
    }
    assert_eq!(h.forwarded.lock().unwrap().len(), 4);
    let resp = h
        .router
        .call(call(
            Method::POST,
            "/v1/update/pause",
            Value::Null,
            Some(TOKEN),
        ))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(parse(&resp)["error"], "operator_port_unreachable");
}

#[tokio::test]
async fn killed_between_the_save_and_the_202_the_retry_gets_the_same_update_id() {
    let h = harness("u8a-r-crash", quick());
    *h.rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
    let resp = h
        .router
        .call(call(Method::POST, U, post_body(B, "r1"), Some(TOKEN)))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no 202 was sent"
    );
    assert!(h.rig.orders.written.lock().unwrap().is_empty());
    // The dead process answers nothing more.
    let (s, _) = send(&h, Method::GET, U, Value::Null).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // A restarted process, same state directory.
    let restarted = router_over(&h.rig, FakeForwarder::default(), quick());
    let resp = restarted
        .call(call(Method::POST, U, post_body(B, "r1"), Some(TOKEN)))
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(parse(&resp)["update_id"], "upd-1");
    assert_eq!(h.rig.orders.written.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_posts_of_different_commits_let_exactly_one_through() {
    let h = Arc::new(harness("u8a-r-race", quick()));
    let mut tasks = Vec::new();
    for i in 0..16 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            let commit = format!("{i:040x}");
            let (s, v) = send(&h, Method::POST, U, post_body(&commit, &format!("r{i}"))).await;
            (s, v["error"].as_str().map(str::to_owned))
        }));
    }
    let mut accepted = 0;
    for t in tasks {
        match t.await.unwrap() {
            (StatusCode::ACCEPTED, None) => accepted += 1,
            (StatusCode::CONFLICT, Some(code)) => assert_eq!(code, "update_open"),
            other => panic!("undefined outcome {other:?}"),
        }
    }
    assert_eq!(accepted, 1);
    assert_eq!(
        h.rig.orders.written.lock().unwrap().len(),
        1,
        "one order, one winner"
    );
}

#[tokio::test]
async fn post_abort_and_rollback_in_one_burst_each_get_a_defined_answer() {
    let h = harness("u8a-r-burst", quick());
    let p = |m, path: &str, body| {
        let req = call(m, path, body, Some(TOKEN));
        h.router.call(req)
    };
    // Arrival order is the order of the futures: join polls them left to right.
    let (a, b, c, d, e, f) = tokio::join!(
        p(Method::POST, "/v1/update/frontend/rollback", Value::Null),
        p(Method::POST, U, post_body(B, "r1")),
        p(Method::POST, U, post_body(COMMIT_C, "r2")),
        p(Method::POST, "/v1/update/frontend/rollback", Value::Null),
        p(Method::POST, "/v1/update/frontend/abort", Value::Null),
        p(Method::POST, "/v1/update/frontend/abort", Value::Null),
    );
    let code = |r: &Response<Bytes>| (r.status(), parse(r)["error"].as_str().map(str::to_owned));
    assert_eq!(
        code(&a),
        (StatusCode::CONFLICT, Some("no_update_open".to_owned()))
    );
    assert_eq!(code(&b), (StatusCode::ACCEPTED, None));
    assert_eq!(
        code(&c),
        (StatusCode::CONFLICT, Some("update_open".to_owned()))
    );
    assert_eq!(
        code(&d),
        (StatusCode::CONFLICT, Some("rollback_refused".to_owned()))
    );
    assert_eq!(code(&e), (StatusCode::OK, None), "the first abort wins");
    assert_eq!(
        code(&f),
        (StatusCode::CONFLICT, Some("no_update_open".to_owned()))
    );
    assert_eq!(h.rig.sup.starts(), 0);
}
