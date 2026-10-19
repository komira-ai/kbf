//! The update API behind the handler: plain values in, plain values out, over the real
//! core with its fakes, with the actor on its own thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use hyper::header::{ALLOW, HeaderValue};
use serde_json::{Value, json};

use super::actor::ActorTiming;
use super::actor_tests::wait_until;
use super::core::CrashPoint;
use super::fakes::{Rig, staged};
use super::handler::{Handler, HandlerReply, HandlerRequest};
use super::router::MAX_BODY;
use crate::proxy::model::Verb;
use crate::testkit::B;

const U: &str = "/v1/update/frontend";
const LONG: Duration = Duration::from_secs(10);

struct Harness {
    rig: Rig,
    handler: Arc<Handler>,
    died: mpsc::Receiver<()>,
}

fn harness(name: &str, timing: ActorTiming) -> Harness {
    let (tx, died) = mpsc::channel();
    let rig = Rig::new(name);
    let handler = Handler::start(rig.core(), timing, move || {
        let _ = tx.send(());
    })
    .unwrap();
    Harness {
        rig,
        handler: Arc::new(handler),
        died,
    }
}

fn call(method: &str, path: &str, body: &Value) -> HandlerRequest {
    HandlerRequest {
        method: method.to_owned(),
        path: path.to_owned(),
        body: if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(body).unwrap()
        },
    }
}

fn post_body(commit: &str, request_id: &str) -> Value {
    json!({"commit": commit, "request_id": request_id, "nodes": true, "rollback": false})
}

fn json_of(reply: &HandlerReply) -> Value {
    serde_json::from_slice(&reply.body).unwrap_or(Value::Null)
}

fn written(rig: &Rig) -> Vec<(Verb, String)> {
    rig.orders
        .written
        .lock()
        .unwrap()
        .iter()
        .map(|l| (l.verb, l.commit.to_string()))
        .collect()
}

#[tokio::test]
async fn a_post_goes_through_the_core_and_writes_the_order() {
    let h = harness("u8b-h-post", ActorTiming::default());
    // No Authorization header, no peer: the caller authenticated before it called.
    let reply = h.handler.handle(call("POST", U, &post_body(B, "r1"))).await;
    assert_eq!(reply.status, 202);
    let body = json_of(&reply);
    assert_eq!(body["phase"], "accepted");
    let update_id = body["update_id"].as_str().unwrap().to_owned();
    assert_eq!(written(&h.rig), [(Verb::FrontendUpdate, B.to_owned())]);
    // The body is JSON and the listener names it: it is not among the extra headers.
    assert!(reply.headers.is_empty(), "{:?}", reply.headers);

    let status = h.handler.handle(call("GET", U, &Value::Null)).await;
    assert_eq!(status.status, 200);
    let status = json_of(&status);
    assert_eq!(status["update_id"], update_id.as_str());
    assert_eq!(status["phase"], "accepted");
}

#[tokio::test]
async fn a_staged_update_is_handed_to_the_supervisor_by_the_next_status_read() {
    let h = harness("u8b-h-status", ActorTiming::default());
    h.handler.handle(call("POST", U, &post_body(B, "r1"))).await;
    h.rig.set_status(staged(B));
    let status = h.handler.handle(call("GET", U, &Value::Null)).await;
    assert_eq!(status.status, 200);
    assert_eq!(json_of(&status)["phase"], "preflight");
    assert_eq!(h.rig.sup.starts(), 1);
}

#[tokio::test]
async fn the_tick_hands_a_staged_update_over_with_no_request_at_all() {
    let timing = ActorTiming {
        tick: Duration::from_millis(5),
        ..ActorTiming::default()
    };
    let h = harness("u8b-h-tick", timing);
    h.handler.handle(call("POST", U, &post_body(B, "r1"))).await;
    h.rig.set_status(staged(B));
    // Nothing asks from here on. The phase the supervisor reports is what a later read
    // shows, so the read below only looks.
    wait_until(|| h.rig.sup.starts() == 1).await;
    let status = h.handler.handle(call("GET", U, &Value::Null)).await;
    assert_eq!(json_of(&status)["phase"], "preflight");
    assert_eq!(h.rig.sup.starts(), 1);
}

#[tokio::test]
async fn an_unknown_path_and_the_operator_port_paths_are_404() {
    let h = harness("u8b-h-404", ActorTiming::default());
    let cases = [
        ("GET", "/"),
        ("GET", "/v1/update/frontend/nope"),
        ("POST", "/v1/update/desired"),
        ("GET", "/v1/update/desired"),
        ("POST", "/v1/update/pause"),
        ("POST", "/v1/update/resume"),
        ("GET", "/v1/update"),
    ];
    for (method, path) in cases {
        let reply = h.handler.handle(call(method, path, &Value::Null)).await;
        assert_eq!(reply.status, 404, "{method} {path}");
        assert_eq!(json_of(&reply)["error"], "not_found", "{method} {path}");
    }
    assert!(written(&h.rig).is_empty());
}

#[tokio::test]
async fn a_wrong_method_is_405_and_the_allow_header_gets_through() {
    let h = harness("u8b-h-405", ActorTiming::default());
    let reply = h.handler.handle(call("PUT", U, &Value::Null)).await;
    assert_eq!(reply.status, 405);
    assert_eq!(
        reply.headers,
        [(ALLOW, HeaderValue::from_static("GET, POST"))]
    );
    let reply = h
        .handler
        .handle(call("GET", "/v1/update/frontend/abort", &Value::Null))
        .await;
    assert_eq!(reply.status, 405);
    assert_eq!(reply.headers, [(ALLOW, HeaderValue::from_static("POST"))]);
}

#[tokio::test]
async fn a_body_over_the_router_cap_is_413_and_nothing_is_run() {
    let h = harness("u8b-h-413", ActorTiming::default());
    let request = HandlerRequest {
        method: "POST".to_owned(),
        path: U.to_owned(),
        body: vec![b' '; MAX_BODY + 1],
    };
    let reply = h.handler.handle(request).await;
    assert_eq!(reply.status, 413);
    assert_eq!(json_of(&reply)["error"], "body_too_large");
    assert!(written(&h.rig).is_empty());
}

#[tokio::test]
async fn a_request_that_is_not_http_is_400() {
    let h = harness("u8b-h-400", ActorTiming::default());
    for (method, path) in [("NOT A METHOD", U), ("GET", "/v1/update /frontend")] {
        let reply = h.handler.handle(call(method, path, &Value::Null)).await;
        assert_eq!(reply.status, 400, "{method} {path}");
        assert_eq!(json_of(&reply)["error"], "bad_request");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_posts_through_the_handler_let_exactly_one_through() {
    let h = harness("u8b-h-race", ActorTiming::default());
    let mut tasks = Vec::new();
    for i in 0..16 {
        let handler = h.handler.clone();
        tasks.push(tokio::spawn(async move {
            let commit = format!("{i:040x}");
            let reply = handler
                .handle(call("POST", U, &post_body(&commit, &format!("r{i}"))))
                .await;
            (
                reply.status,
                json_of(&reply)["error"].as_str().map(str::to_owned),
            )
        }));
    }
    let mut accepted = 0;
    for t in tasks {
        match t.await.unwrap() {
            (202, None) => accepted += 1,
            (409, Some(code)) => assert_eq!(code, "update_open"),
            other => panic!("undefined outcome {other:?}"),
        }
    }
    assert_eq!(accepted, 1);
    assert_eq!(written(&h.rig).len(), 1, "one order, one winner");
}

/// The discriminating test for the actor's thread: with the actor stuck in a call, the
/// runtime the caller is on keeps running (a heartbeat task counts it), and a caller that
/// asks gets its 503 at the deadline. On a current-thread runtime, an actor that ran as a
/// task there would block the heartbeat and the caller's own timer together.
#[tokio::test]
async fn a_stuck_actor_stalls_neither_the_callers_runtime_nor_past_the_deadline() {
    let timing = ActorTiming {
        deadline: Duration::from_millis(300),
        ..ActorTiming::default()
    };
    let h = harness("u8b-h-stuck", timing);
    let stall = h.rig.status.stall();
    stall.close();
    let post = tokio::spawn({
        let handler = h.handler.clone();
        async move { handler.handle(call("POST", U, &post_body(B, "r1"))).await }
    });
    stall.arrived(1).await;

    let beats = Arc::new(AtomicUsize::new(0));
    let heart = tokio::spawn({
        let beats = beats.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                beats.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    let started = Instant::now();
    let reply = h.handler.handle(call("GET", U, &Value::Null)).await;
    let waited = started.elapsed();
    heart.abort();

    assert_eq!(
        reply.status,
        503,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(json_of(&reply)["error"], "update_service_unavailable");
    assert!(waited < Duration::from_secs(2), "waited {waited:?}");
    let beats = beats.load(Ordering::SeqCst);
    assert!(beats >= 5, "the runtime ran {beats} beats in {waited:?}");
    // The call in hand was given up by its caller at the deadline too, and still finishes.
    assert_eq!(post.await.unwrap().status, 503);
    stall.open();
    wait_until(|| written(&h.rig).len() == 1).await;
    // The 503 did not mean "not done": the retry with the same request id is answered with
    // the update the first call opened, and no second order is written.
    let open = json_of(&h.handler.handle(call("GET", U, &Value::Null)).await);
    let retry = h.handler.handle(call("POST", U, &post_body(B, "r1"))).await;
    assert!(
        (200..300).contains(&retry.status),
        "{}",
        String::from_utf8_lossy(&retry.body)
    );
    assert_eq!(json_of(&retry)["update_id"], open["update_id"]);
    assert_eq!(written(&h.rig).len(), 1);
}

#[tokio::test]
async fn an_actor_that_crashed_is_a_503_and_the_caller_is_told_to_stop_once() {
    let h = harness("u8b-h-crash", ActorTiming::default());
    *h.rig.die.lock().unwrap() = Some(CrashPoint::AfterRequestSaved);
    let reply = h.handler.handle(call("POST", U, &post_body(B, "r1"))).await;
    assert_eq!(reply.status, 503, "no 202 was sent");
    assert_eq!(
        h.died.recv_timeout(LONG),
        Ok(()),
        "the stop callback did not run"
    );
    for _ in 0..3 {
        let reply = h.handler.handle(call("GET", U, &Value::Null)).await;
        assert_eq!(reply.status, 503);
    }
    assert_eq!(
        h.died.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Disconnected),
        "the stop callback ran again"
    );
    assert!(written(&h.rig).is_empty());
}

#[tokio::test]
async fn the_handler_is_shared_and_its_future_is_send() {
    fn shared<T: Send + Sync>() {}
    fn send<T: Send>(_: &T) {}
    shared::<Handler>();
    let h = harness("u8b-h-send", ActorTiming::default());
    let future = h.handler.handle(call("GET", U, &Value::Null));
    send(&future);
    assert_eq!(future.await.status, 200);
}
