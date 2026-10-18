//! How long a stream lives, and how it ends: idle timeouts, permits held to the last half,
//! client and child resets.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use h2::Reason;
use http::HeaderMap;
use tokio::time::Instant;

use super::kit::{CAPS, EXECUTE, OPERATOR, Rig, finish, grpc_ok, read_all, script, tagged, until};
use crate::slot::Slot;

const HOUR: Duration = Duration::from_secs(3600);

/// A child that answers after `delay`.
fn slow(delay: Duration) -> super::kit::Script {
    script(move |mut call| async move {
        read_all(&mut call.body).await;
        tokio::time::sleep(delay).await;
        grpc_ok(&mut call.respond, "late");
    })
}

#[tokio::test(start_paused = true)]
async fn idle_3h_paused_not_closed() {
    let rig = Rig::start(slow(3 * HOUR + Duration::from_secs(1)), tagged("b")).await;
    let start = Instant::now();
    let reply = rig.client().await.unary(EXECUTE).await;
    assert_eq!(reply.text(), "late", "an Execute is never idle");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert!(start.elapsed() >= 3 * HOUR);
    until(|| rig.core.slots.open_streams() == 0).await;
}

#[tokio::test(start_paused = true)]
async fn any_other_stream_is_closed_after_7200_s_of_silence() {
    let rig = Rig::start(slow(3 * HOUR), tagged("b")).await;
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    let waited = start.elapsed();
    assert!(
        waited >= 2 * HOUR && waited < 2 * HOUR + Duration::from_secs(5),
        "closed at the idle bound, not before and not 3 hours later: {waited:?}"
    );
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
}

#[tokio::test(start_paused = true)]
async fn no_stream_deadline_3h_paused() {
    // A server stream that sends a message every hour for 3 hours is never idle for 2.
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .expect("response");
        let mut send = call
            .respond
            .send_response(response, false)
            .expect("headers");
        for _ in 0..3 {
            tokio::time::sleep(HOUR).await;
            send.send_data(Bytes::from_static(b"m"), false)
                .expect("data");
        }
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
        send.send_trailers(trailers).expect("trailers");
    });
    let rig = Rig::start(a, tagged("b")).await;
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.body, b"mmm");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert!(reply.error.is_none());
    assert!(start.elapsed() >= 3 * HOUR);
}

#[tokio::test(start_paused = true)]
async fn open_permits_stays_1_until_upstream_rst() {
    // The child answers in full while the client's request is still open.
    let a = script(|mut call| async move {
        let _ = call.body.data().await;
        grpc_ok(&mut call.respond, "early");
        // Holds the request half open; the connection carries on.
        std::future::pending::<()>().await;
    });
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(EXECUTE, false).await;
    body.send_data(Bytes::from_static(b"part"), false)
        .expect("body");
    let reply = finish(response).await;
    assert_eq!(reply.text(), "early", "the response is complete");
    // Give every task the chance to drop the permit if it was going to.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        rig.core.slots.open_permits(Slot::A),
        1,
        "the response is over, the request half is not: the permit stays"
    );
    assert_eq!(
        rig.core.slots.retire(Slot::B),
        crate::slot::Retire::NeverOpened
    );
    rig.flip_to_b();
    assert_eq!(rig.core.slots.retire(Slot::A), crate::slot::Retire::Busy(1));
    // The client ends its request: the stream is finished both ways and the permit drops.
    body.send_data(Bytes::new(), true).expect("end");
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
    assert_eq!(rig.core.slots.retire(Slot::A), crate::slot::Retire::Closed);
}

#[tokio::test(start_paused = true)]
async fn a_client_reset_reaches_the_child_as_cancel_and_frees_the_permit() {
    let reason = Arc::new(Mutex::new(None));
    let a = {
        let reason = Arc::clone(&reason);
        script(move |mut call| {
            let reason = Arc::clone(&reason);
            async move {
                let _ = call.body.data().await;
                let got = std::future::poll_fn(|cx| call.respond.poll_reset(cx)).await;
                *reason.lock().expect("lock") = got.ok();
            }
        })
    };
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"part"), false)
        .expect("body");
    until(|| rig.core.slots.open_permits(Slot::A) == 1).await;
    body.send_reset(Reason::CANCEL);
    drop(response);
    until(|| reason.lock().expect("lock").is_some()).await;
    assert_eq!(*reason.lock().expect("lock"), Some(Reason::CANCEL));
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
    until(|| rig.core.slots.open_streams() == 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_child_reset_before_headers_is_unavailable_and_not_repeated() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        call.respond.send_reset(Reason::INTERNAL_ERROR);
    });
    let rig = Rig::start(a, tagged("b")).await;
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert!(reply.body.is_empty());
    assert_eq!(
        rig.a.state.dispatches(),
        1,
        "an INTERNAL_ERROR is not a safe failure"
    );
}

fn mid_body_reset() -> super::kit::Script {
    script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .expect("response");
        let mut send = call
            .respond
            .send_response(response, false)
            .expect("headers");
        send.send_data(Bytes::from_static(b"part"), false)
            .expect("data");
        // Let the proxy forward the first frame before the reset arrives.
        tokio::time::sleep(Duration::from_secs(1)).await;
        send.send_reset(Reason::INTERNAL_ERROR);
    })
}

#[tokio::test(start_paused = true)]
async fn a_child_reset_mid_body_ends_a_grpc_stream_with_a_non_ok_trailer() {
    let rig = Rig::start(mid_body_reset(), tagged("b")).await;
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(reply.body, b"part", "the data before the cut is delivered");
    assert_eq!(
        reply.grpc_status().as_deref(),
        Some("14"),
        "and the stream ends non-OK"
    );
    assert!(reply.trailers.is_some());
    assert!(reply.error.is_none(), "a trailer, not a reset");
    until(|| rig.core.slots.open_streams() == 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_child_reset_mid_body_on_http_is_a_reset_never_a_grpc_trailer() {
    let mut rig = Rig::start(mid_body_reset(), tagged("b")).await;
    let _ = &mut rig;
    let reply = rig.client().await.plain("GET", OPERATOR, b"").await;
    assert_eq!(reply.status, Some(200));
    assert!(reply.error.is_some(), "the stream is reset: {reply:?}");
    assert!(reply.trailers.is_none());
    assert_eq!(rig.core.metrics.panics.load(Ordering::Acquire), 0);
}

#[tokio::test(start_paused = true)]
async fn a_request_with_trailers_reaches_the_child_with_them() {
    let seen = Arc::new(Mutex::new(None));
    let a = {
        let seen = Arc::clone(&seen);
        script(move |mut call| {
            let seen = Arc::clone(&seen);
            async move {
                let body = read_all(&mut call.body).await;
                let trailers = call.body.trailers().await.ok().flatten();
                *seen.lock().expect("lock") = Some((body, trailers));
                grpc_ok(&mut call.respond, "a");
            }
        })
    };
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"data"), false)
        .expect("data");
    let mut trailers = HeaderMap::new();
    trailers.insert("x-sum", http::HeaderValue::from_static("4"));
    body.send_trailers(trailers).expect("trailers");
    assert_eq!(finish(response).await.text(), "a");
    let got = seen
        .lock()
        .expect("lock")
        .take()
        .expect("the child saw the request");
    assert_eq!(got.0, b"data");
    assert_eq!(got.1.expect("trailers").get("x-sum").expect("x-sum"), "4");
}
