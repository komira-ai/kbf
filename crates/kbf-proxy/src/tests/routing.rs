//! Per-stream routing: a flip moves new streams and leaves bound ones alone.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use h2::Reason;
use http::HeaderValue;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::kit::{
    CAPS, Rig, Script, WRITE, finish, grpc_ok, next_of, read_all, script, tagged, until,
};
use crate::slot::Slot;

const HOLD: Duration = Duration::from_secs(20);

#[tokio::test(start_paused = true)]
async fn flip_200_streams_complete_on_a() {
    let gate = Arc::new(Semaphore::new(0));
    let a: Script = {
        let gate = Arc::clone(&gate);
        script(move |mut call| {
            let gate = Arc::clone(&gate);
            async move {
                read_all(&mut call.body).await;
                gate.acquire().await.expect("open").forget();
                grpc_ok(&mut call.respond, "a");
            }
        })
    };
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;

    let mut open = JoinSet::new();
    for _ in 0..200 {
        let (response, mut body) = client.start(CAPS, false).await;
        body.send_data(Bytes::from_static(b"x"), true)
            .expect("body");
        open.spawn(finish(response));
    }
    until(|| rig.a.state.dispatches() == 200).await;
    assert_eq!(rig.core.slots.open_permits(Slot::A), 200);

    // The swap, as a supervisor runs it: hold, flip, release.
    rig.core.slots.open(Slot::B);
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    rig.core.slots.release_into(Slot::B).expect("B is open");
    drop(guard);

    // A stream after the flip goes to B, on the same connection, while A's 200 are open.
    let after = client.unary(CAPS).await;
    assert_eq!(after.text(), "b");
    assert_eq!(rig.b.state.dispatches(), 1);
    assert_eq!(rig.a.state.dispatches(), 200);
    assert!(
        client.alive(),
        "the flip did not close the client's connection"
    );

    gate.add_permits(200);
    let mut done = 0;
    while let Some(reply) = next_of(&mut open).await {
        assert!(
            reply.error.is_none(),
            "no RST_STREAM and no GOAWAY: {:?}",
            reply.error
        );
        assert_eq!(reply.status, Some(200));
        assert_eq!(reply.text(), "a", "{reply:?}");
        assert_eq!(reply.grpc_status().as_deref(), Some("0"));
        done += 1;
    }
    assert_eq!(done, 200);
    assert!(client.alive());
    client
        .send
        .clone()
        .ready()
        .await
        .expect("no GOAWAY was received");
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
    assert_eq!(rig.core.slots.retire(Slot::A), crate::slot::Retire::Closed);
}

#[tokio::test(start_paused = true)]
async fn one_connection_binds_per_stream() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    assert_eq!(client.unary(CAPS).await.text(), "a");
    rig.flip_to_b();
    assert_eq!(
        client.unary(CAPS).await.text(),
        "b",
        "the same connection, a new stream"
    );
    assert_eq!(client.unary(CAPS).await.text(), "b");
    assert_eq!(rig.a.state.conns_total.load(Ordering::Acquire), 1);
    assert_eq!(rig.b.state.conns_total.load(Ordering::Acquire), 1);
}

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

#[tokio::test(start_paused = true)]
async fn a_refused_unary_stream_is_dispatched_once_more_with_its_whole_body() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let a = {
        let bodies = Arc::clone(&bodies);
        script(move |mut call| {
            let bodies = Arc::clone(&bodies);
            async move {
                let body = read_all(&mut call.body).await;
                bodies.lock().expect("lock").push(body);
                if call.nth == 1 {
                    call.respond.send_reset(Reason::REFUSED_STREAM);
                } else {
                    grpc_ok(&mut call.respond, "second");
                }
            }
        })
    };
    let rig = Rig::start(a, tagged("b")).await;
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.text(), "second");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert_eq!(rig.a.state.dispatches(), 2);
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    assert_eq!(
        *bodies.lock().expect("lock"),
        vec![b"ping".to_vec(), b"ping".to_vec()],
        "the second dispatch carries the same request, replayed from the buffer"
    );
    until(|| rig.core.slots.open_streams() == 0).await;
}

#[tokio::test(start_paused = true)]
async fn a_refused_request_without_a_body_is_dispatched_once_more() {
    let rig = Rig::start(refusing(1, "second"), tagged("b")).await;
    let mut client = rig.client().await;
    let (response, _body) = client.start(CAPS, true).await;
    let reply = finish(response).await;
    assert_eq!(reply.text(), "second");
    assert_eq!(rig.a.state.dispatches(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_second_refusal_is_not_dispatched_again() {
    let rig = Rig::start(refusing(10, "never"), tagged("b")).await;
    let reply = rig.client().await.unary(CAPS).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"), "UNAVAILABLE");
    assert_eq!(rig.a.state.dispatches(), 2, "the first and one more");
    until(|| rig.core.slots.open_streams() == 0).await;
    assert_eq!(rig.core.slots.open_permits(Slot::A), 0);
}

#[tokio::test(start_paused = true)]
async fn bytestream_not_retried_after_first_frame() {
    let rig = Rig::start(refusing(10, "never"), tagged("b")).await;
    let mut client = rig.client().await;
    // The whole request (one frame and END_STREAM) is in the buffer, so only the ByteStream
    // rule stops a second dispatch.
    let (response, mut body) = client.start(WRITE, false).await;
    body.send_data(Bytes::from_static(b"first frame"), true)
        .expect("body");
    let reply = finish(response).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(
        rig.a.state.dispatches(),
        1,
        "a ByteStream is dispatched once"
    );
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 0);
}

#[tokio::test(start_paused = true)]
async fn a_request_still_open_is_not_dispatched_again() {
    // The child refuses at once, before the client has finished its request.
    let a = script(|mut call| async move {
        call.respond.send_reset(Reason::REFUSED_STREAM);
    });
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"part"), false)
        .expect("body");
    let reply = finish(response).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(
        rig.a.state.dispatches(),
        1,
        "the request was not whole, so no replay"
    );
    drop(body);
}

#[tokio::test(start_paused = true)]
async fn a_dead_child_answers_unavailable_and_a_whole_request_is_dialed_twice() {
    let mut rig = Rig::start(tagged("a"), tagged("b")).await;
    rig.a.stop_accepting().await;
    let mut client = rig.client().await;
    let metric = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Acquire);
    // A request with a body has not been read when the connect fails: nothing is buffered to
    // replay, so there is one dial and the client's own retry takes over.
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.status, Some(200));
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(metric(&rig.core.metrics.upstream_dials), 1);
    // A request that was only headers is whole already: dialed once more, then given up.
    let (response, _body) = client.start(CAPS, true).await;
    let reply = finish(response).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    assert_eq!(
        metric(&rig.core.metrics.upstream_dials),
        3,
        "one dial, then two"
    );
    assert_eq!(metric(&rig.core.metrics.upstream_dial_failed), 3);
    until(|| rig.core.slots.open_streams() == 0).await;
    assert_eq!(
        rig.core.slots.open_permits(Slot::A),
        0,
        "the permit is released"
    );
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
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (first, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let first = tokio::spawn(finish(first));
    until(|| rig.a.state.dispatches() == 1).await;
    assert_eq!(client.unary(CAPS).await.text(), "second");
    // Let the proxy's connection take the GOAWAY in.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(client.unary(CAPS).await.text(), "third");
    assert_eq!(
        rig.a.state.conns_total.load(Ordering::Acquire),
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
async fn retire_a_restart_a_first_stream_succeeds() {
    let mut rig = Rig::start(tagged("old"), tagged("b")).await;
    let mut client = rig.client().await;
    assert_eq!(client.unary(CAPS).await.text(), "old");
    let old = Arc::clone(&rig.a.state);
    assert_eq!(old.conns_open(), 1);

    // The old child was told to stop and listens no more; its connection is still up.
    rig.a.stop_accepting().await;
    rig.flip_to_b();
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
    assert_eq!(rig.core.slots.retire(Slot::A), crate::slot::Retire::Closed);
    assert_eq!(rig.core.slots.generation(Slot::A), 1);

    // A new child in the same slot: a new generation.
    rig.respawn(Slot::A, &tagged("new"));
    assert_eq!(rig.core.slots.open(Slot::A), 2);
    rig.core.slots.set_active(Slot::A).expect("A is open");

    let reply = client.unary(CAPS).await;
    assert_eq!(
        reply.text(),
        "new",
        "the first stream met the new child, not the old connection"
    );
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    // The old generation's pool is gone and its connection with it.
    until(|| old.conns_open() == 0).await;
}

#[tokio::test(start_paused = true)]
async fn inbound_identity_headers_never_reach_the_child() {
    let a: Script = script(|mut call| async move {
        read_all(&mut call.body).await;
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .header("x-kbf-fenced", "nothing-committed")
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
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client
        .start_with(
            CAPS,
            false,
            &[
                ("x-kbf-node-id", "an-impostor"),
                ("x-kbf-role", "daemon"),
                ("x-kbf-proxy-auth", "guess"),
                ("x-trace", "kept"),
            ],
        )
        .await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let reply = finish(response).await;

    let seen = rig.a.state.headers_of(CAPS);
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].keys().all(|k| !k.as_str().starts_with("x-kbf-")),
        "{:?}",
        seen[0]
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
    let rig = Rig::start(gated(&gate), tagged("b")).await;
    // The child takes two streams per connection.
    rig.a.state.max_concurrent.store(2, Ordering::Release);
    let mut client = rig.client().await;
    let mut open = start_many(&mut client, 6).await;
    // All six are in the child at once, on at least three connections, not two at a time on one.
    until(|| rig.a.state.dispatches() == 6).await;
    assert!(rig.a.state.conns_total.load(Ordering::Acquire) >= 3);
    gate.add_permits(6);
    while let Some(reply) = next_of(&mut open).await {
        assert_eq!(reply.text(), "a");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_beyond_the_connection_cap_waits_and_keeps_its_permit() {
    let gate = Arc::new(Semaphore::new(0));
    let rig = Rig::start(gated(&gate), tagged("b")).await;
    // One stream per connection, so 256 connections (the cap) carry 256 streams.
    rig.a.state.max_concurrent.store(1, Ordering::Release);
    let mut client = rig.client().await;
    let mut open = start_many(&mut client, 257).await;
    until(|| rig.a.state.dispatches() == 256).await;
    assert_eq!(rig.a.state.conns_total.load(Ordering::Acquire), 256);
    assert_eq!(
        rig.core.slots.open_permits(Slot::A),
        257,
        "the waiting one keeps its permit"
    );
    // Nothing more reaches the child until a stream finishes and frees its place.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        rig.a.state.dispatches(),
        256,
        "the 257th waits for a free stream"
    );
    gate.add_permits(1);
    until(|| rig.a.state.dispatches() == 257).await;
    gate.add_permits(300);
    while let Some(reply) = next_of(&mut open).await {
        assert_eq!(reply.text(), "a");
    }
    until(|| rig.core.slots.open_permits(Slot::A) == 0).await;
}

#[tokio::test(start_paused = true)]
async fn goaway_below_last_stream_id_retries_the_in_flight_stream() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    // The first connection the child takes answers the first stream with GOAWAY(last = 0):
    // the stream is above it, so the child provably did not process it.
    rig.a.state.raw_goaway_first.store(true, Ordering::Release);
    let mut client = rig.client().await;
    let reply = client.unary(CAPS).await;
    assert_eq!(reply.text(), "a", "dispatched again, on a new connection");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    assert_eq!(
        rig.a.state.conns_total.load(Ordering::Acquire),
        1,
        "only the second is a real child"
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
}
