//! How long a stream lives, and how it ends: idle timeouts, permits held to the last half,
//! client and child resets.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use h2::Reason;
use http::HeaderMap;
use tokio::time::Instant;

use super::kit::{CAPS, EXECUTE, OPERATOR, WRITE, finish, grpc_ok, read_all, script, tagged, until};
use super::pool_kit::Rig;
use crate::pool::member_addr;
use crate::route::Target;

const HOUR: Duration = Duration::from_secs(3600);

/// A child that answers after `delay`.
fn slow(delay: Duration) -> super::kit::Script {
    script(move |mut call| async move {
        read_all(&mut call.body).await;
        tokio::time::sleep(delay).await;
        grpc_ok(&mut call.respond, "late");
    })
}

/// The streams open on member `a`'s REAPI listener.
fn open_on_a(rig: &Rig) -> u64 {
    rig.members.snapshot()[0].open_streams.reapi
}

/// Asserts that an `Execute` that is answered after 3 hours is not ended by the idle bound, and
/// that its place is given back once it has been answered.
#[tokio::test(start_paused = true)]
async fn idle_3h_paused_not_closed() {
    let rig = Rig::start(&[slow(3 * HOUR + Duration::from_secs(1))]);
    let start = Instant::now();
    let reply = rig.client().await.unary(EXECUTE).await;
    assert_eq!(reply.text(), "late", "an Execute is never idle");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert!(start.elapsed() >= 3 * HOUR);
    until(|| rig.core.open_streams() == 0).await;
}

/// Asserts that any call other than an `Execute` that gets no answer for 2 hours is ended
/// `UNAVAILABLE` at the idle bound, not before and not when the child would have answered, and
/// that its place on the member is given back.
#[tokio::test(start_paused = true)]
async fn any_other_stream_is_closed_after_7200_s_of_silence() {
    let rig = Rig::start(&[slow(3 * HOUR)]);
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    let waited = start.elapsed();
    assert!(waited >= 2 * HOUR, "before the idle bound: {waited:?}");
    assert!(waited < 2 * HOUR + Duration::from_secs(5), "{waited:?}");
    until(|| open_on_a(&rig) == 0).await;
}

/// Asserts that no deadline ends a stream that keeps moving: a server stream with a message an
/// hour and a `ByteStream` write with a chunk an hour both run for 3 hours and end in `OK`.
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
    let rig = Rig::start(&[a]);
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.body, b"mmm");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert!(reply.error.is_none());
    assert!(start.elapsed() >= 3 * HOUR);

    // A ByteStream Write whose upload takes 3 hours, a chunk an hour: no timer of the
    // proxy's ends a stream that is moving, however long it runs.
    let a = script(|mut call| async move {
        let body = read_all(&mut call.body).await;
        assert_eq!(body, b"www");
        grpc_ok(&mut call.respond, "written");
    });
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let start = Instant::now();
    let (response, mut body) = client.start(WRITE, false).await;
    for chunk in 0..3 {
        tokio::time::sleep(HOUR).await;
        body.send_data(Bytes::from_static(b"w"), chunk == 2)
            .expect("data");
    }
    let reply = finish(response).await;
    assert_eq!(reply.text(), "written");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert!(reply.error.is_none());
    assert!(start.elapsed() >= 3 * HOUR);
}

/// Asserts that a member's place for a stream is kept after the child has answered in full
/// while the client's request is still open, and is given back only when the client ends its
/// request.
#[tokio::test(start_paused = true)]
async fn the_place_on_a_member_is_kept_until_both_halves_are_finished() {
    // The child answers in full while the client's request is still open.
    let a = script(|mut call| async move {
        let _ = call.body.data().await;
        grpc_ok(&mut call.respond, "early");
        // Holds the request half open; the connection carries on.
        std::future::pending::<()>().await;
    });
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(EXECUTE, false).await;
    body.send_data(Bytes::from_static(b"part"), false)
        .expect("body");
    let reply = finish(response).await;
    assert_eq!(reply.text(), "early", "the response is complete");
    // Give every task the chance to drop the place if it was going to.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        open_on_a(&rig),
        1,
        "the response is over, the request half is not: the place stays"
    );
    // The client ends its request: the stream is finished both ways and the place is given back.
    body.send_data(Bytes::new(), true).expect("end");
    until(|| open_on_a(&rig) == 0).await;
}

/// Asserts that a client that resets its stream reaches the child as a `CANCEL`, and that the
/// stream's place on the member and its count are given back.
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
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"part"), false)
        .expect("body");
    until(|| open_on_a(&rig) == 1).await;
    body.send_reset(Reason::CANCEL);
    drop(response);
    until(|| reason.lock().expect("lock").is_some()).await;
    assert_eq!(*reason.lock().expect("lock"), Some(Reason::CANCEL));
    until(|| open_on_a(&rig) == 0).await;
    until(|| rig.core.open_streams() == 0).await;
}

/// Asserts that a child that resets a stream with `INTERNAL_ERROR` before its headers is
/// answered `UNAVAILABLE` and that the stream is not given to the other member, which would
/// run a call the child may have started twice.
#[tokio::test(start_paused = true)]
async fn a_child_reset_before_headers_is_unavailable_and_not_repeated() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        call.respond.send_reset(Reason::INTERNAL_ERROR);
    });
    let rig = Rig::start(&[a, tagged("b")]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert!(reply.body.is_empty());
    assert_eq!(
        rig.dispatches(),
        [1, 0],
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

/// Asserts that a child that resets a gRPC stream after some data has the data delivered and the
/// stream ended by a non-OK trailer, not by a reset.
#[tokio::test(start_paused = true)]
async fn a_child_reset_mid_body_ends_a_grpc_stream_with_a_non_ok_trailer() {
    let rig = Rig::start(&[mid_body_reset()]);
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
    until(|| rig.core.open_streams() == 0).await;
}

/// Asserts that a child that resets a stream of an HTTP route after some data has the stream
/// reset for the client, with no gRPC trailer made up, and that nothing panics.
#[tokio::test(start_paused = true)]
async fn a_child_reset_mid_body_on_http_is_a_reset_never_a_grpc_trailer() {
    let rig = Rig::start(&[mid_body_reset()]);
    let reply = rig.client().await.plain("GET", OPERATOR, b"").await;
    assert_eq!(reply.status, Some(200));
    assert!(reply.error.is_some(), "the stream is reset: {reply:?}");
    assert!(reply.trailers.is_none());
    assert_eq!(rig.core.metrics.panics.load(Ordering::Acquire), 0);
}

/// Asserts that a request that ends in trailers reaches the child with its data and its
/// trailers.
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
    let rig = Rig::start(&[a]);
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

/// Asserts that a response that goes silent after some data is ended at the idle bound, with
/// the data delivered, a non-OK trailer, and its place on the member given back.
#[tokio::test(start_paused = true)]
async fn a_response_that_goes_silent_is_ended_at_the_idle_bound() {
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
        send.send_data(Bytes::from_static(b"m"), false)
            .expect("data");
        // Then nothing, for 3 hours, with the stream open.
        tokio::time::sleep(3 * HOUR).await;
        drop(send);
    });
    let rig = Rig::start(&[a]);
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(
        reply.body, b"m",
        "what came before the silence is delivered"
    );
    assert_eq!(
        reply.grpc_status().as_deref(),
        Some("14"),
        "and the stream ends non-OK"
    );
    let waited = start.elapsed();
    assert!(waited >= 2 * HOUR, "{waited:?}");
    assert!(waited < 2 * HOUR + Duration::from_secs(5), "{waited:?}");
    until(|| open_on_a(&rig) == 0).await;
}

/// Asserts that a child that answers in full and then stops the upload with a reset ends the
/// request pump, and the stream's place on the member is given back.
#[tokio::test(start_paused = true)]
async fn a_child_that_answers_early_and_stops_the_upload_ends_the_pump() {
    let a = script(|mut call| async move {
        let _ = call.body.data().await;
        grpc_ok(&mut call.respond, "early");
        // After the proxy has taken the answer in, the child stops the upload.
        tokio::time::sleep(Duration::from_secs(1)).await;
        call.respond.send_reset(Reason::NO_ERROR);
    });
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(EXECUTE, false).await;
    body.send_data(Bytes::from_static(b"first"), false)
        .expect("body");
    assert_eq!(finish(response).await.text(), "early");
    tokio::time::sleep(Duration::from_secs(2)).await;
    // The client has more to upload; the child has stopped listening.
    body.reserve_capacity(100_000);
    let granted = std::future::poll_fn(|cx| body.poll_capacity(cx)).await;
    assert!(granted.is_some());
    let _ = body.send_data(Bytes::from(vec![9_u8; 100_000]), false);
    until(|| open_on_a(&rig) == 0).await;
    assert_eq!(rig.core.open_streams(), 0);
}

/// Asserts that a member nothing can connect to fails the stream at the connect bound of 3
/// seconds, `UNAVAILABLE`, and that the failed dial is counted.
#[tokio::test(start_paused = true)]
async fn a_child_that_never_accepts_fails_the_stream_at_the_connect_timeout() {
    let rig = Rig::start(&[tagged("a")]);
    let reapi = member_addr(rig.members.base(0), Target::Reapi);
    rig.net.blackhole(reapi);
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    let waited = start.elapsed();
    assert!(waited >= Duration::from_secs(3), "{waited:?}");
    assert!(waited < Duration::from_secs(4), "{waited:?}");
    assert_eq!(
        rig.core
            .metrics
            .upstream_dial_failed
            .load(Ordering::Acquire),
        1
    );
}

/// Asserts that an upload that sends a chunk an hour is not idle, and ends in an answer after 3
/// hours.
#[tokio::test(start_paused = true)]
async fn a_slow_upload_is_not_idle_while_it_keeps_sending() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        grpc_ok(&mut call.respond, "done");
    });
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(WRITE, false).await;
    // One chunk an hour for three hours: silent for 1 h at a time, never for 2 h.
    for _ in 0..3 {
        tokio::time::sleep(HOUR).await;
        body.send_data(Bytes::from_static(b"x"), false)
            .expect("body");
    }
    body.send_data(Bytes::new(), true).expect("end");
    let reply = finish(response).await;
    assert_eq!(reply.text(), "done", "{reply:?}");
}

/// Asserts that a child that takes a call and never answers has it ended `UNAVAILABLE` at one
/// idle bound, and that the call is not given to the other member, because the child may have
/// started it.
#[tokio::test(start_paused = true)]
async fn a_silent_child_ends_the_stream_at_one_idle_bound_and_is_not_dispatched_again() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        std::future::pending::<()>().await;
    });
    let rig = Rig::start(&[a, tagged("b")]);
    let start = Instant::now();
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert!(
        start.elapsed() < 2 * HOUR + Duration::from_secs(100),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(
        rig.core.metrics.retries.load(Ordering::Acquire),
        0,
        "an idle bound is not a safe failure: the child may have started the call"
    );
    assert_eq!(rig.dispatches(), [1, 0]);
}

/// Asserts that a request pump parked on a child's full flow-control window lets go when the
/// response half gives up at the idle bound, so the stream's place on the member is given back.
#[tokio::test(start_paused = true)]
async fn a_pump_parked_on_a_full_upstream_window_ends_with_the_response() {
    // The child answers the head, then never reads: its window (4 MiB) fills, and the proxy's
    // request pump parks waiting for credit that never comes.
    let a = script(|mut call| async move {
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .expect("a response");
        let _send = call.respond.send_response(response, false).expect("head");
        std::future::pending::<()>().await;
    });
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(WRITE, false).await;
    let answer = tokio::spawn(finish(response));
    // More than the child's window; the client stops when the proxy stops reading it.
    let sender = tokio::spawn(async move {
        for _ in 0..(6 << 4) {
            let mut chunk = Bytes::from(vec![7_u8; 64 * 1024]);
            while !chunk.is_empty() {
                body.reserve_capacity(chunk.len());
                let granted = std::future::poll_fn(|cx| body.poll_capacity(cx)).await;
                let Some(Ok(granted)) = granted else { return };
                if body.send_data(chunk.split_to(granted), false).is_err() {
                    return;
                }
            }
        }
    });
    // Silent for the idle bound: the response half gives up, and the pump must let go.
    let reply = answer.await.expect("client");
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    until(|| open_on_a(&rig) == 0).await;
    assert_eq!(rig.core.open_streams(), 0);
    sender.abort();
}
