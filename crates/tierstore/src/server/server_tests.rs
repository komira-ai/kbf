//! The probes of the server, its count of requests in flight, and its phases.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use crate::Op;

use super::REFUSING;
use super::auth::sign;
use super::testing::{BUCKET, Reply, Rig};

const MEBIBYTE: u64 = 1 << 20;

/// Sends a request without a signature.
async fn unsigned(rig: &Rig, method: &str, path: &str) -> Reply {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Full::new(Bytes::new()))
        .unwrap();
    rig.send_raw(request).await
}

/// Asserts that `/healthz` and `/readyz` answer a request without a signature, only to GET and
/// HEAD; that `/readyz` says 503 from the moment the drain begins, `/healthz` goes on saying
/// 200, and the S3 calls are still served.
#[tokio::test(start_paused = true)]
async fn the_probes_answer_without_a_signature_and_readyz_follows_the_drain() {
    let rig = Rig::new(MEBIBYTE).await;
    let health = unsigned(&rig, "GET", "/healthz").await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.body, "ok");
    assert_eq!(health.header("content-type"), "text/plain");
    let ready = unsigned(&rig, "GET", "/readyz").await;
    assert_eq!(ready.status, StatusCode::OK);
    assert_eq!(ready.body, "ready");
    let head = unsigned(&rig, "HEAD", "/readyz").await;
    assert_eq!(head.status, StatusCode::OK);
    for (method, path) in [("POST", "/readyz"), ("GET", "/healthz2")] {
        let refused = unsigned(&rig, method, path).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert!(rig.server.is_ready());
    rig.server.begin_drain();
    rig.server.begin_drain();
    assert!(!rig.server.is_ready());
    let draining = unsigned(&rig, "GET", "/readyz").await;
    assert_eq!(draining.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(draining.body, "draining");
    let alive = unsigned(&rig, "GET", "/healthz").await;
    assert_eq!(alive.status, StatusCode::OK);
    assert_eq!(rig.put("k", b"v").await.status, StatusCode::OK);
}

/// Asserts that a server that has stopped accepting connections still serves the S3 calls of the
/// requests it reads, each answered with `Connection: close` and none refused, answers the
/// probes, and that a second call to begin the drain does not take it back to an earlier phase.
#[tokio::test(start_paused = true)]
async fn a_server_past_the_settle_serves_s3_calls_with_connection_close() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.server.phase.store(REFUSING, Ordering::SeqCst);
    let served = rig.put("k", b"v").await;
    assert_eq!(served.status, StatusCode::OK);
    assert_eq!(served.header("connection"), "close");
    let head = rig.call("HEAD", "k", b"").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("connection"), "close");
    let health = unsigned(&rig, "GET", "/healthz").await;
    assert_eq!(health.status, StatusCode::OK);
    let ready = unsigned(&rig, "GET", "/readyz").await;
    assert_eq!(ready.status, StatusCode::SERVICE_UNAVAILABLE);
    rig.server.begin_drain();
    assert_eq!(rig.server.phase(), REFUSING);
}

/// Asserts that no answer says `Connection: close` while the server is serving, and that every
/// answer does from the moment the drain begins, the probes and the refusals of the server's
/// own (a bad signature) included. It would catch a header set at a later phase than the first
/// one, and one set on the successes only.
#[tokio::test(start_paused = true)]
async fn the_answers_say_close_from_the_moment_the_drain_begins() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("k", b"v").await.header("connection"), "");
    assert_eq!(
        unsigned(&rig, "GET", "/readyz").await.header("connection"),
        ""
    );
    rig.server.begin_drain();
    let put = rig.put("j", b"v").await;
    assert_eq!(put.status, StatusCode::OK);
    assert_eq!(put.header("connection"), "close");
    assert_eq!(
        rig.call("GET", "k", b"").await.header("connection"),
        "close"
    );
    let ready = unsigned(&rig, "GET", "/readyz").await;
    assert_eq!(ready.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(ready.header("connection"), "close");
    let unsigned_put = unsigned(&rig, "PUT", &format!("/{BUCKET}/x")).await;
    assert_eq!(unsigned_put.status, StatusCode::FORBIDDEN);
    assert_eq!(unsigned_put.header("connection"), "close");
}

/// Asserts that a request that is being answered is counted until it is done, that waiting for
/// the requests in flight returns at once when there are none and not before the one that
/// runs is done, and that the request finishes with its own answer.
#[tokio::test(start_paused = true)]
async fn requests_in_flight_are_counted_and_waited_for() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.server.in_flight(), 0);
    let idle = rig.server.wait_idle();
    let waited = tokio::time::timeout(Duration::from_secs(1), idle).await;
    assert!(waited.is_ok());
    rig.hot.set_delay(Op::Put, Some(Duration::from_secs(10)));
    let uri = format!("/{BUCKET}/k");
    let request = sign(&rig.config, "PUT", &uri, &[], b"v");
    let server = Arc::clone(&rig.server);
    let task = tokio::spawn(async move { server.handle(request).await.status() });
    while rig.server.in_flight() == 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(rig.server.in_flight(), 1);
    let busy = tokio::time::timeout(Duration::from_secs(5), rig.server.wait_idle()).await;
    assert!(busy.is_err());
    rig.server.wait_idle().await;
    assert_eq!(rig.server.in_flight(), 0);
    assert_eq!(task.await.unwrap(), StatusCode::OK);
}
