//! Per-stream placement: a stream is placed on a member and keeps it, and a stream a member
//! fails before an answer is placed again on another member only when it is safe to.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use h2::Reason;
use http::HeaderValue;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::kit::{
    CAPS, RawFrame, RawPeer, Script, WRITE, finish, grpc_ok, next_of, read_all, script, tagged,
    until,
};
use super::pool_kit::Rig;
use crate::pool::member_addr;
use crate::route::Target;

/// Refuses the first `refusals` streams it accepts, after reading the whole request.
fn refusing(refusals: usize, then: &'static str) -> Script {
    script(move |mut call| async move {
        read_all(&mut call.body).await;
        if call.nth <= refusals {
            call.respond.send_reset(Reason::REFUSED_STREAM);
        } else {
            grpc_ok(&mut call.respond, then);
        }
    })
}

/// Asserts that a stream a member refuses is placed once more, on the next member, with its
/// whole request replayed from the buffer.
#[tokio::test(start_paused = true)]
async fn a_refused_unary_stream_is_placed_once_more_with_its_whole_body() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recording = |refuse: bool, then: &'static str| {
        let bodies = Arc::clone(&bodies);
        script(move |mut call| {
            let bodies = Arc::clone(&bodies);
            async move {
                let body = read_all(&mut call.body).await;
                bodies.lock().expect("lock").push(body);
                if refuse {
                    call.respond.send_reset(Reason::REFUSED_STREAM);
                } else {
                    grpc_ok(&mut call.respond, then);
                }
            }
        })
    };
    let rig = Rig::start(&[recording(true, "first"), recording(false, "second")]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.text(), "second");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert_eq!(rig.dispatches(), [1, 1]);
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    assert_eq!(
        *bodies.lock().expect("lock"),
        vec![b"ping".to_vec(), b"ping".to_vec()],
        "the second placement carries the same request, replayed from the buffer"
    );
    until(|| rig.core.open_streams() == 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_refused_request_without_a_body_is_placed_once_more() {
    let rig = Rig::start(&[refusing(1, "first"), tagged("second")]);
    let mut client = rig.client().await;
    let (response, _body) = client.start(CAPS, true).await;
    let reply = finish(response).await;
    assert_eq!(reply.text(), "second");
    assert_eq!(rig.dispatches(), [1, 1]);
}

#[tokio::test(start_paused = true)]
async fn a_second_refusal_is_not_placed_again() {
    let rig = Rig::start(&[refusing(10, "never"), refusing(10, "never")]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "UNAVAILABLE");
    assert_eq!(rig.dispatches(), [1, 1], "the first and one more");
    until(|| rig.core.open_streams() == 0).await;
    assert_eq!(rig.members.open_total(), 0);
}

#[tokio::test(start_paused = true)]
async fn bytestream_not_retried_after_first_frame() {
    let rig = Rig::start(&[refusing(10, "never"), tagged("b")]);
    let mut client = rig.client().await;
    // The whole request (one frame and END_STREAM) is in the buffer, so only the ByteStream
    // rule stops a second placement.
    let (response, mut body) = client.start(WRITE, false).await;
    body.send_data(Bytes::from_static(b"first frame"), true)
        .expect("body");
    let reply = finish(response).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(rig.dispatches(), [1, 0], "a ByteStream is dispatched once");
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 0);
}

#[tokio::test(start_paused = true)]
async fn a_request_still_open_is_not_placed_again() {
    // The member refuses at once, before the client has finished its request.
    let a = script(|mut call| async move {
        call.respond.send_reset(Reason::REFUSED_STREAM);
    });
    let rig = Rig::start(&[a, tagged("b")]);
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"part"), false)
        .expect("body");
    // A bound, not a hang: a pump left running would never let the stream go.
    let reply = tokio::time::timeout(Duration::from_secs(5), finish(response))
        .await
        .expect("the stream was not answered");
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(
        rig.dispatches(),
        [1, 0],
        "the request was not whole, so no replay"
    );
    drop(body);
}

/// Asserts that a member nothing listens on answers `UNAVAILABLE`, that a stream is dialed once
/// for each member that is asked for it, and that the place on the member is given back.
#[tokio::test(start_paused = true)]
async fn a_dead_member_answers_unavailable_and_is_dialed_once_for_each_stream() {
    let mut rig = Rig::start(&[tagged("a")]);
    rig.children[0].stop_accepting().await;
    let mut client = rig.client().await;
    let metric = |c: &AtomicU64| c.load(Ordering::Acquire);
    // A request with a body has not been read when the connect fails: nothing is buffered to
    // replay, so there is one dial and the client's own retry takes over.
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(metric(&rig.core.metrics.upstream_dials), 1);
    // A request that was only headers is whole already, but the member that failed it is not
    // asked for it again and no other member is left: one more dial, then given up.
    let (response, _body) = client.start(CAPS, true).await;
    let reply = finish(response).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(metric(&rig.core.metrics.upstream_dials), 2);
    assert_eq!(metric(&rig.core.metrics.upstream_dial_failed), 2);
    until(|| rig.core.open_streams() == 0).await;
    assert_eq!(rig.members.open_total(), 0, "the place is given back");
}

#[tokio::test(start_paused = true)]
async fn a_child_goaway_is_never_forwarded_and_the_next_stream_dials_again() {
    let hang = Arc::new(Semaphore::new(0));
    let a: Script = {
        let hang = Arc::clone(&hang);
        script(move |mut call| {
            let hang = Arc::clone(&hang);
            async move {
                read_all(&mut call.body).await;
                match call.nth {
                    // Stays open, so the connection outlives its GOAWAY.
                    1 => {
                        hang.acquire().await.expect("open").forget();
                        grpc_ok(&mut call.respond, "first");
                    }
                    2 => {
                        call.goaway.notify_one();
                        grpc_ok(&mut call.respond, "second");
                    }
                    _ => grpc_ok(&mut call.respond, "third"),
                }
            }
        })
    };
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (first, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let first = tokio::spawn(finish(first));
    until(|| rig.a().state.dispatches() == 1).await;
    assert_eq!(client.unary(CAPS).await.text(), "second");
    // Let the proxy's connection take the GOAWAY in.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(client.unary(CAPS).await.text(), "third");
    assert_eq!(
        rig.a().state.conns_total.load(Ordering::Acquire),
        2,
        "a new connection"
    );
    assert!(
        client.alive(),
        "the child's GOAWAY did not reach the client"
    );
    client
        .send
        .clone()
        .ready()
        .await
        .expect("the client got no GOAWAY");
    hang.add_permits(1);
    let first = first.await.expect("client");
    assert_eq!(first.text(), "first", "the stream in flight finished");
}

#[tokio::test(start_paused = true)]
async fn inbound_identity_headers_never_reach_the_child() {
    let a: Script = script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .header("x-kbf-secret", "child-private")
            .header("x-keep", "yes")
            .body(())
            .expect("response");
        let mut send = call
            .respond
            .send_response(response, false)
            .expect("headers");
        send.send_data(Bytes::from_static(b"ok"), false)
            .expect("data");
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("0"));
        trailers.insert("x-kbf-trailer", HeaderValue::from_static("child-private"));
        send.send_trailers(trailers).expect("trailers");
    });
    let rig = Rig::start(&[a]);
    let mut client = rig.client().await;
    let (response, mut body) = client
        .start_with(
            CAPS,
            false,
            &[
                ("x-kbf-node-id", "an-impostor"),
                ("x-kbf-role", "daemon"),
                ("x-kbf-proxy-auth", "guess"),
                ("x-kbf-client-addr", "203.0.113.66"),
                ("x-trace", "kept"),
            ],
        )
        .await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let reply = finish(response).await;

    let seen = rig.a().state.headers_of(CAPS);
    assert_eq!(seen.len(), 1);
    let reserved: Vec<_> = seen[0]
        .iter()
        .filter(|(k, _)| k.as_str().starts_with("x-kbf-"))
        .collect();
    assert_eq!(
        reserved.len(),
        2,
        "nothing the client forged survives; only the proxy's secret and its reading of the \
         client's address are set: {reserved:?}"
    );
    assert_eq!(
        seen[0].get_all("x-kbf-client-addr").iter().count(),
        1,
        "one value"
    );
    assert_eq!(
        seen[0].get("x-kbf-client-addr").map(HeaderValue::as_bytes),
        Some(super::kit::CLIENT_IP.to_string().as_bytes()),
        "the connection's address, not the forged one"
    );
    assert_eq!(
        seen[0].get("x-kbf-proxy-auth").map(HeaderValue::as_bytes),
        Some(hex::encode(super::kit::SECRET).as_bytes())
    );
    assert_eq!(
        seen[0].get("x-trace").map(HeaderValue::as_bytes),
        Some(&b"kept"[..])
    );
    assert!(
        reply
            .headers
            .keys()
            .all(|k| !k.as_str().starts_with("x-kbf-"))
    );
    assert_eq!(
        reply.headers.get("x-keep").map(HeaderValue::as_bytes),
        Some(&b"yes"[..])
    );
    let trailers = reply.trailers.expect("trailers");
    assert!(trailers.keys().all(|k| !k.as_str().starts_with("x-kbf-")));
    assert_eq!(reply.body, b"ok");
}

/// A child that holds every stream until `gate` has a permit for it.
fn gated(gate: &Arc<Semaphore>) -> Script {
    let gate = Arc::clone(gate);
    script(move |mut call| {
        let gate = Arc::clone(&gate);
        async move {
            read_all(&mut call.body).await;
            gate.acquire().await.expect("open").forget();
            grpc_ok(&mut call.respond, "a");
        }
    })
}

/// Starts `n` unary requests on `client` without waiting for their answers.
async fn start_many(client: &mut super::kit::Client, n: usize) -> JoinSet<super::kit::Reply> {
    let mut open = JoinSet::new();
    for _ in 0..n {
        let (response, mut body) = client.start(CAPS, false).await;
        body.send_data(Bytes::from_static(b"x"), true)
            .expect("body");
        open.spawn(finish(response));
    }
    open
}

#[tokio::test(start_paused = true)]
async fn the_pool_grows_instead_of_queueing_behind_the_childs_stream_limit() {
    let gate = Arc::new(Semaphore::new(0));
    let rig = Rig::start(&[gated(&gate)]);
    // The child takes two streams per connection.
    rig.a().state.max_concurrent.store(2, Ordering::Release);
    let mut client = rig.client().await;
    let mut open = start_many(&mut client, 6).await;
    // All six are in the child at once, on at least three connections, not two at a time on one.
    until(|| rig.a().state.dispatches() == 6).await;
    assert!(rig.a().state.conns_total.load(Ordering::Acquire) >= 3);
    gate.add_permits(6);
    while let Some(reply) = next_of(&mut open).await {
        assert_eq!(reply.text(), "a");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_beyond_the_connection_cap_waits_and_keeps_its_place() {
    let gate = Arc::new(Semaphore::new(0));
    let rig = Rig::start(&[gated(&gate)]);
    // One stream per connection, so 256 connections (the cap) carry 256 streams.
    rig.a().state.max_concurrent.store(1, Ordering::Release);
    let mut client = rig.client().await;
    let mut open = start_many(&mut client, 257).await;
    until(|| rig.a().state.dispatches() == 256).await;
    assert_eq!(rig.a().state.conns_total.load(Ordering::Acquire), 256);
    let open_on_a = || rig.members.snapshot()[0].open_streams.reapi;
    assert_eq!(open_on_a(), 257, "the waiting one keeps its place");
    // Nothing more reaches the child until a stream finishes and frees its place.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        rig.a().state.dispatches(),
        256,
        "the 257th waits for a free stream"
    );
    gate.add_permits(1);
    until(|| rig.a().state.dispatches() == 257).await;
    gate.add_permits(300);
    while let Some(reply) = next_of(&mut open).await {
        assert_eq!(reply.text(), "a");
    }
    until(|| open_on_a() == 0).await;
}

/// Asserts that a member that answers the first stream of a connection with a GOAWAY below the
/// stream's id has the stream placed again on the other member, and that the GOAWAY is not
/// forwarded to the client.
#[tokio::test(start_paused = true)]
async fn goaway_below_last_stream_id_places_the_in_flight_stream_on_the_next_member() {
    let rig = Rig::start(&[tagged("a"), tagged("b")]);
    // The first connection member `a` takes answers the first stream with GOAWAY(last = 0):
    // the stream is above it, so the child provably did not process it.
    let (first, second) = (&rig.children[0].state, &rig.children[1].state);
    first.raw_goaway_first.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.text(), "b", "placed again, on the other member");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    assert_eq!(first.conns_total.load(Ordering::Acquire), 0);
    assert_eq!(second.conns_total.load(Ordering::Acquire), 1);
    assert!(
        client.alive(),
        "the child's GOAWAY did not reach the client"
    );
    client
        .send
        .clone()
        .ready()
        .await
        .expect("the client got no GOAWAY");
}

/// Asserts that a GOAWAY the proxy's own library raised, on a protocol error of a member's
/// connection, is not proof that the member never took the stream: the stream is not placed
/// again (on the other member it could run twice).
#[tokio::test(start_paused = true)]
async fn a_library_goaway_is_not_a_proof_that_the_child_never_took_the_stream() {
    let rig = Rig::start(&[tagged("a"), tagged("b")]);
    // The child takes the first stream, then breaks the protocol: h2 raises a GOAWAY of its own
    // on every stream, the one the child received included.
    let broken = &rig.children[0].state;
    broken.raw_garbage_first.store(true, Ordering::Release);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(
        rig.core.metrics.retries.load(Ordering::Acquire),
        0,
        "dispatched again after a GOAWAY the proxy's own library raised: {reply:?}"
    );
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert_eq!(rig.dispatches(), [0, 0]);
    assert_eq!(rig.a().state.conns_total.load(Ordering::Acquire), 0);
}

#[tokio::test(start_paused = true)]
async fn a_dial_cancelled_by_a_client_reset_gives_back_its_dial_slot() {
    let rig = Rig::start(&[tagged("a")]);
    let reapi = member_addr(rig.members.base(0), Target::Reapi);
    rig.net.blackhole(reapi);
    let mut client = rig.client().await;
    // More cancelled dials than the pool allows at once: each leaked slot would count.
    for dials in 1..=u64::try_from(crate::pool::MAX_DIALS).expect("small") {
        let (_response, mut body) = client.start(CAPS, false).await;
        until(|| rig.core.metrics.upstream_dials.load(Ordering::Acquire) == dials).await;
        body.send_reset(Reason::CANCEL);
        until(|| rig.core.open_streams() == 0).await;
    }
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let reply = tokio::time::timeout(Duration::from_secs(30), finish(response))
        .await
        .expect("the next stream was never answered: the pool is wedged");
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
}

#[tokio::test(start_paused = true)]
async fn a_burst_dials_at_most_16_at_once_and_opens_at_most_256_connections() {
    let gate = Arc::new(Semaphore::new(0));
    let rig = Rig::start(&[gated(&gate)]);
    rig.a().state.max_concurrent.store(1, Ordering::Release);
    rig.net.delay_connects(Duration::from_millis(500));
    let mut client = rig.client().await;
    // One stream first, so the waves of 16 dials do not line up with the 256 cap: the last
    // wave has room for 15 only, and a dial count that forgot the dials in flight sends 16.
    let mut open = start_many(&mut client, 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(rig.a().state.dispatches(), 1);
    let mut rest = start_many(&mut client, 299).await;
    // 256 connections at 16 per half second take 8 s; the clock runs ahead only when idle.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(rig.a().state.dispatches(), 256);
    assert_eq!(
        rig.net.peak_connects(),
        crate::pool::MAX_DIALS,
        "dials in flight at once: parallel, up to the cap and no more"
    );
    assert_eq!(
        rig.core.metrics.upstream_dials.load(Ordering::Acquire),
        256,
        "no dial past the connection cap, however many were in flight"
    );
    gate.add_permits(400);
    while let Some(reply) = next_of(&mut open).await {
        assert_eq!(reply.text(), "a");
    }
    while let Some(reply) = next_of(&mut rest).await {
        assert_eq!(reply.text(), "a");
    }
}

/// Asserts that a member that never answers the dial's PING is not there: the connection is
/// given up after the connect bound, and the whole request is placed on the other member.
#[tokio::test(start_paused = true)]
async fn a_member_that_never_answers_the_ping_is_not_there() {
    let rig = Rig::start(&[tagged("a"), tagged("b")]);
    let mute = &rig.children[0].state;
    *mute.raw_pongs_first.lock().expect("lock") = Some(0);
    let mut client = rig.client().await;
    let (response, _body) = client.start(CAPS, true).await;
    let reply = finish(response).await;
    assert_eq!(reply.text(), "b", "{reply:?}");
    assert_eq!(
        rig.core
            .metrics
            .upstream_dial_failed
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(rig.core.metrics.upstream_dials.load(Ordering::Acquire), 2);
}

#[tokio::test(start_paused = true)]
async fn a_response_larger_than_the_stream_window_arrives_whole() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .expect("a response");
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("0"));
        let mut send = call.respond.send_response(response, false).expect("head");
        send.send_data(Bytes::from(vec![5_u8; 1 << 20]), false)
            .expect("data");
        send.send_trailers(trailers).expect("trailers");
    });
    let rig = Rig::start(&[a]);
    let reply = tokio::time::timeout(Duration::from_secs(1), rig.client().await.unary(CAPS))
        .await
        .expect("a response of 1 MiB completes: credit is returned as frames are read");
    assert_eq!(reply.body.len(), 1 << 20);
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
}

/// Sends `frames` DATA frames of `size` bytes each, then one byte with END_STREAM.
async fn send_frames(
    client: &mut super::kit::Client,
    frames: usize,
    size: usize,
) -> h2::client::ResponseFuture {
    let (response, mut body) = client.start(CAPS, false).await;
    for _ in 0..frames {
        body.send_data(Bytes::from(vec![1_u8; size]), false)
            .expect("body");
    }
    body.send_data(Bytes::from_static(b"z"), true).expect("end");
    response
}

#[tokio::test(start_paused = true)]
async fn a_request_of_many_tiny_frames_is_not_kept_for_a_replay() {
    // The child reads the whole request, then refuses it: a replay is safe by the stream's
    // state, but the window is 64 frames, however few bytes they are. (h2 itself ends a
    // connection that sends about a hundred tiny frames, so the cap sits under that.)
    let rig = Rig::start(&[refusing(1, "first"), tagged("second")]);
    let mut client = rig.client().await;
    let response = send_frames(&mut client, 80, 1).await;
    let reply = finish(response).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 0);
    assert_eq!(rig.dispatches(), [1, 0]);
}

#[tokio::test(start_paused = true)]
async fn a_few_small_frames_are_replayed_whole() {
    let rig = Rig::start(&[refusing(1, "first"), tagged("second")]);
    let mut client = rig.client().await;
    let response = send_frames(&mut client, 40, 1).await;
    let reply = finish(response).await;
    assert_eq!(reply.text(), "second", "{reply:?}");
}

#[tokio::test(start_paused = true)]
async fn a_response_that_ends_with_its_data_ends_without_trailers() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .expect("a response");
        let mut send = call.respond.send_response(response, false).expect("head");
        send.send_data(Bytes::from_static(b"whole"), true)
            .expect("data and END_STREAM");
    });
    let rig = Rig::start(&[a]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.text(), "whole", "{reply:?}");
    assert!(
        reply.trailers.is_none(),
        "no status was invented: {reply:?}"
    );
}

/// Catches a response whose last DATA frame is a separate, empty one (END_STREAM without a
/// trailer): the body ends there as well, with no status invented, now that the body reports
/// the end of the upstream stream by itself.
#[tokio::test(start_paused = true)]
async fn a_response_ended_by_an_empty_data_frame_ends_without_trailers() {
    let a = script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .expect("a response");
        let mut send = call.respond.send_response(response, false).expect("head");
        send.send_data(Bytes::from_static(b"part"), false)
            .expect("data");
        // After the proxy has passed the data on and is waiting for what follows.
        tokio::time::sleep(Duration::from_secs(1)).await;
        send.send_data(Bytes::new(), true).expect("END_STREAM");
    });
    let rig = Rig::start(&[a]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.text(), "part", "{reply:?}");
    assert!(reply.trailers.is_none(), "{reply:?}");
}

/// A child that answers every stream, after reading the request, with a trailers-only
/// `UNAVAILABLE` (headers with END_STREAM) that has an `x-kbf-fenced` header.
fn unavailable_with_a_fenced_header() -> Script {
    script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .header("grpc-status", "14")
            .header("x-kbf-fenced", "nothing-committed")
            .body(())
            .expect("a response");
        let _ = call.respond.send_response(response, true);
    })
}

/// Asserts that a trailers-only `UNAVAILABLE` with an `x-kbf-fenced` header is not taken for a
/// failure that is safe to repeat: the client gets it once with the header removed, the
/// request is not given to the other member, and no retry is counted.
#[tokio::test(start_paused = true)]
async fn an_answer_with_a_fenced_header_is_forwarded_once_without_the_header() {
    let rig = Rig::start(&[unavailable_with_a_fenced_header(), tagged("b")]);
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{reply:?}");
    let leaked = reply
        .headers
        .keys()
        .chain(reply.trailers.iter().flat_map(http::HeaderMap::keys))
        .any(|name| name.as_str().starts_with("x-kbf-"));
    assert!(!leaked, "an x-kbf-* header reached the client: {reply:?}");
    assert_eq!(rig.dispatches(), [1, 0]);
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 0);
    until(|| rig.core.open_streams() == 0).await;
}

/// Asserts that a trailers-only answer from a member reaches the client as one HEADERS frame
/// that carries END_STREAM, because a client that reads `grpc-status` from the last HEADERS
/// frame finds none after a bare head and an empty DATA frame.
#[tokio::test(start_paused = true)]
async fn a_forwarded_trailers_only_answer_ends_the_stream_on_its_headers() {
    let rig = Rig::start(&[unavailable_with_a_fenced_header()]);
    let mut peer = RawPeer::connect(&rig).await;
    peer.send(&RawPeer::grpc_request(1, CAPS)).await;
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = peer.next().await {
            if frame.kind == RawFrame::HEADERS {
                return Some(frame);
            }
        }
        None
    })
    .await
    .expect("no answer")
    .expect("the connection closed");
    assert_eq!(rig.a().state.dispatches(), 1);
    assert_eq!(
        head.flags & RawFrame::END_STREAM,
        RawFrame::END_STREAM,
        "the head of a trailers-only answer carries END_STREAM"
    );
}
