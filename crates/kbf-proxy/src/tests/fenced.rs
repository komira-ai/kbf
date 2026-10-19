//! The fenced-not-committed answer (C6): a trailers-only `UNAVAILABLE` with exactly one
//! `x-kbf-fenced: nothing-committed` is dispatched again, as a new bind, if the request is
//! whole and unanswered; in every other case the client gets the same `UNAVAILABLE` without
//! the marker.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use h2::Reason;
use h2::server::SendResponse;
use http::{HeaderMap, HeaderValue, Response};
use tokio::sync::Semaphore;

use super::kit::{
    CAPS, EXECUTE, RawFrame, RawPeer, Reply, Rig, Script, WRITE, finish, grpc_ok, read_all, script,
    tagged, until,
};
use crate::slot::Slot;

const HOLD: Duration = Duration::from_secs(20);

/// The fenced answer: headers with END_STREAM, status 14, one marker.
fn send_fenced(respond: &mut SendResponse<Bytes>) {
    let response = Response::builder()
        .status(200)
        .header("content-type", "application/grpc")
        .header("grpc-status", "14")
        .header("x-kbf-fenced", "nothing-committed")
        .body(())
        .expect("a response");
    let _ = respond.send_response(response, true);
}

/// Reads the whole request, then answers fenced to the first `fenced` streams and `then` to
/// the rest.
fn fencing(fenced: usize, then: &'static str) -> Script {
    script(move |mut call| async move {
        read_all(&mut call.body).await;
        if call.nth <= fenced {
            send_fenced(&mut call.respond);
        } else {
            grpc_ok(&mut call.respond, then);
        }
    })
}

/// No `x-kbf-*` header reached the client, in the headers or the trailers.
fn marker_stripped(reply: &Reply) {
    let all = reply
        .headers
        .keys()
        .chain(reply.trailers.iter().flat_map(HeaderMap::keys));
    for name in all {
        assert!(
            !name.as_str().starts_with("x-kbf-"),
            "{name} reached the client"
        );
    }
}

/// Catches a fenced answer that reaches the client when the request could be sent again (the
/// build would be told `UNAVAILABLE` for a call that had no effect), a re-dispatch with less
/// than the whole request, and a window closed before the fenced check (the answer would be
/// read as an ordinary one).
#[tokio::test(start_paused = true)]
async fn fenced_trailers_only_redispatched() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let a = {
        let bodies = Arc::clone(&bodies);
        script(move |mut call| {
            let bodies = Arc::clone(&bodies);
            async move {
                let body = read_all(&mut call.body).await;
                bodies.lock().expect("lock").push(body);
                if call.nth == 1 {
                    send_fenced(&mut call.respond);
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
    marker_stripped(&reply);
    assert_eq!(rig.a.state.dispatches(), 2);
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);
    assert_eq!(
        *bodies.lock().expect("lock"),
        vec![b"ping".to_vec(), b"ping".to_vec()],
        "the whole request, replayed"
    );
    until(|| rig.core.slots.open_streams() == 0).await;
    assert_eq!(rig.core.slots.open_permits(Slot::A), 0);
}

/// One call to a child that always answers fenced: the reply and how often it was dispatched.
async fn always_fenced(path: &str, body: usize) -> (Reply, usize) {
    let rig = Rig::start(fencing(usize::MAX, "never"), tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut send) = client.start(path, false).await;
    send.send_data(Bytes::from(vec![7_u8; body]), true)
        .expect("body");
    let reply = finish(response).await;
    until(|| rig.core.slots.open_streams() == 0).await;
    (reply, rig.a.state.dispatches())
}

/// Catches a fenced answer forwarded with its marker, a fenced answer retried when the window
/// is closed (two dispatches are the most; a ByteStream is never repeated; a request over the
/// 64 KB buffer cannot be replayed whole), and a client left without an answer. The client
/// sees the `UNAVAILABLE` it would have seen without the marker, which it retries itself.
#[tokio::test(start_paused = true)]
async fn fenced_with_closed_window_reaches_client_as_unavailable() {
    for (what, path, body, dispatches) in [
        ("both dispatches were fenced", CAPS, 4, 2),
        ("a ByteStream is dispatched once", WRITE, 4, 1),
        ("the request outgrew the buffer", CAPS, 70 * 1024, 1),
    ] {
        let (reply, dispatched) = always_fenced(path, body).await;
        assert_eq!(reply.status, Some(200), "{what}");
        assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{what}");
        assert!(reply.error.is_none(), "{what}: {:?}", reply.error);
        marker_stripped(&reply);
        assert_eq!(dispatched, dispatches, "{what}");
    }
}

/// Catches a bidirectional call being dispatched again with part of its request: its body is
/// still open when the child refuses or fences, so nothing of it may be spliced onto a new
/// upstream stream. The client gets its `UNAVAILABLE` and retries the call itself.
#[tokio::test(start_paused = true)]
async fn an_open_bidirectional_request_is_not_redispatched() {
    for fenced in [false, true] {
        // Answers before the client has finished.
        let a = script(move |mut call| async move {
            if fenced {
                send_fenced(&mut call.respond);
            } else {
                call.respond.send_reset(Reason::REFUSED_STREAM);
            }
        });
        let rig = Rig::start(a, tagged("b")).await;
        let mut client = rig.client().await;
        let (response, mut body) = client.start(EXECUTE, false).await;
        body.send_data(Bytes::from_static(b"hello"), false)
            .expect("body");
        // A bound, not a hang: a pump left running would never let the stream go.
        let reply = tokio::time::timeout(Duration::from_secs(5), finish(response))
            .await
            .expect("the stream was not answered");
        assert_eq!(
            reply.grpc_status().as_deref(),
            Some("14"),
            "fenced: {fenced}"
        );
        marker_stripped(&reply);
        assert_eq!(rig.a.state.dispatches(), 1, "fenced: {fenced}");
        assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 0);
        drop(body);
    }
}

/// Catches the fenced check taking anything for the fenced answer: a head with a body behind
/// it (the child may have run), two markers, or another value of the marker. Each reaches the
/// client as the child sent it, minus the `x-kbf-*` headers, after one dispatch.
#[tokio::test(start_paused = true)]
async fn a_fenced_look_alike_is_forwarded_and_never_dispatched_again() {
    for shape in ["body", "two markers", "other value"] {
        let a = script(move |mut call| async move {
            read_all(&mut call.body).await;
            let mut builder = Response::builder()
                .status(200)
                .header("content-type", "application/grpc")
                .header("grpc-status", "14");
            let value = if shape == "other value" {
                "yes"
            } else {
                "nothing-committed"
            };
            builder = builder.header("x-kbf-fenced", value);
            if shape == "two markers" {
                builder = builder.header("x-kbf-fenced", "nothing-committed");
            }
            let response = builder.body(()).expect("a response");
            if shape == "body" {
                let mut send = call
                    .respond
                    .send_response(response, false)
                    .expect("headers");
                let _ = send.send_data(Bytes::from_static(b"partial"), false);
                let mut trailers = HeaderMap::new();
                trailers.insert("grpc-status", HeaderValue::from_static("14"));
                trailers.insert(
                    "x-kbf-fenced",
                    HeaderValue::from_static("nothing-committed"),
                );
                let _ = send.send_trailers(trailers);
            } else {
                let _ = call.respond.send_response(response, true);
            }
        });
        let rig = Rig::start(a, tagged("b")).await;
        let reply = rig.client().await.unary(CAPS).await;
        assert_eq!(reply.grpc_status().as_deref(), Some("14"), "{shape}");
        marker_stripped(&reply);
        assert_eq!(
            rig.a.state.dispatches(),
            1,
            "{shape}: not fenced, so not repeated"
        );
        assert_eq!(
            rig.core.metrics.retries.load(Ordering::Acquire),
            0,
            "{shape}"
        );
    }
}

/// Catches a re-dispatch that goes back to the slot the stream was bound to. The stream is
/// bound to A before a hold, A is quiesced and answers fenced after the hold began: the retry
/// is a new bind, so it waits in the hold queue like any other stream and lands in B, the slot
/// the hold ends into, and A is dispatched to once.
#[tokio::test(start_paused = true)]
async fn a_stream_bound_pre_hold_dispatched_post_quiesce_then_fenced_lands_on_b() {
    let quiesced = Arc::new(Semaphore::new(0));
    let a: Script = {
        let quiesced = Arc::clone(&quiesced);
        script(move |mut call| {
            let quiesced = Arc::clone(&quiesced);
            async move {
                read_all(&mut call.body).await;
                if call.nth == 1 {
                    quiesced.acquire().await.expect("open").forget();
                    send_fenced(&mut call.respond);
                } else {
                    grpc_ok(&mut call.respond, "a again");
                }
            }
        })
    };
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let call = tokio::spawn(finish(response));
    until(|| rig.a.state.dispatches() == 1).await;

    // The swap, as a supervisor runs it: B opens, the gate holds, A is told to stop.
    rig.core.slots.open(Slot::B);
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    quiesced.add_permits(1);
    until(|| rig.core.queue.held() == 1).await;
    assert!(!call.is_finished(), "the retry waits for the hold to end");
    // A retry is a new bind: the old permit is gone, so A can retire, and the stream is still
    // in a count, so the supervisor never sees no stream while it waits.
    assert_eq!(
        rig.core.slots.open_permits(Slot::A),
        0,
        "A's permit was kept"
    );
    assert_eq!(
        rig.core.slots.open_streams(),
        1,
        "the stream was in no count"
    );
    assert_eq!(rig.b.state.dispatches(), 0);
    assert_eq!(rig.core.metrics.retries.load(Ordering::Acquire), 1);

    rig.core.slots.release_into(Slot::B).expect("B is open");
    drop(guard);
    let reply = call.await.expect("client");
    assert_eq!(reply.text(), "b");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert_eq!(rig.a.state.dispatches(), 1, "A was not tried again");
    assert_eq!(rig.b.state.dispatches(), 1);
    until(|| rig.core.slots.open_streams() == 0).await;
}

/// Catches a retry that starts a new hold bound. A stream held for 12 s of a 20 s bound,
/// released into B and fenced by B during a second hold, waits in the second hold only until
/// the first `since` plus the bound (20 s after it was first held), not for a fresh 20 s.
#[tokio::test(start_paused = true)]
async fn a_retried_stream_keeps_the_first_hold_bound() {
    let fence = Arc::new(Semaphore::new(0));
    let b: Script = {
        let fence = Arc::clone(&fence);
        script(move |mut call| {
            let fence = Arc::clone(&fence);
            async move {
                read_all(&mut call.body).await;
                fence.acquire().await.expect("open").forget();
                send_fenced(&mut call.respond);
            }
        })
    };
    let rig = Rig::start(tagged("a"), b).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let call = tokio::spawn(finish(response));
    until(|| rig.core.queue.held() == 1).await;
    tokio::time::sleep(Duration::from_secs(12)).await;

    rig.core.slots.open(Slot::B);
    rig.core.slots.release_into(Slot::B).expect("B is open");
    drop(guard);
    until(|| rig.b.state.dispatches() == 1).await;
    let second = rig.core.slots.hold(HOLD).expect("no hold in force");
    fence.add_permits(1);
    until(|| rig.core.queue.held() == 1).await;
    // 12 s + 7 s in: 1 s of the first bound is left; a fresh bound would run to 32 s.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(!call.is_finished(), "answered before the bound");
    let reply = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the retry got a new bound")
        .expect("client");
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    drop(second);
}

/// Catches a forwarded trailers-only answer whose HEADERS frame does not end the stream: the
/// upstream's fenced answer, forwarded when no re-dispatch is left, must reach the client as one
/// HEADERS frame with END_STREAM, because a client that reads `grpc-status` from the last
/// HEADERS frame finds none after a bare head and an empty DATA frame.
#[tokio::test(start_paused = true)]
async fn a_forwarded_trailers_only_answer_ends_the_stream_on_its_headers() {
    let rig = Rig::start(fencing(usize::MAX, "never"), tagged("b")).await;
    let mut peer = RawPeer::connect(&rig).await;
    peer.send(&RawPeer::grpc_request(1, WRITE)).await;
    let head = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = peer.next().await {
            if frame.kind == RawFrame::HEADERS && frame.stream == 1 {
                return Some(frame);
            }
        }
        None
    })
    .await
    .expect("no answer")
    .expect("the connection closed");
    assert_eq!(
        rig.a.state.dispatches(),
        1,
        "a ByteStream is dispatched once"
    );
    assert_eq!(
        head.flags & RawFrame::END_STREAM,
        RawFrame::END_STREAM,
        "the head of a trailers-only answer carries END_STREAM"
    );
}
