//! The hold gate end to end: streams wait, buffer at most the window, bind into the slot a
//! release names, and fail `UNAVAILABLE` at their bound.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use tokio::task::JoinSet;

use super::kit::{CAPS, Reply, Rig, finish, grpc_ok, next_of, read_all, script, tagged, until};
use crate::hold::STREAM_WINDOW_BYTES;
use crate::slot::Slot;

const HOLD: Duration = Duration::from_secs(20);

fn unavailable(reply: &Reply) -> bool {
    reply.status == Some(200)
        && reply.grpc_status().as_deref() == Some("14")
        && reply.body.is_empty()
        && reply.error.is_none()
}

#[tokio::test(start_paused = true)]
async fn hold_1000_streams_bounded_and_expire() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let mut streams = JoinSet::new();
    for _ in 0..1000 {
        let (response, _body) = client.start(CAPS, true).await;
        streams.spawn(finish(response));
    }
    until(|| rig.core.queue.held() == 1000).await;
    assert_eq!(
        rig.a.state.dispatches(),
        0,
        "nothing opened upstream while held"
    );
    assert_eq!(
        rig.core.slots.open_permits(Slot::A),
        0,
        "held streams are not bound"
    );
    assert_eq!(rig.core.slots.open_streams(), 1000, "but they are counted");

    tokio::time::advance(HOLD).await;
    let mut failed = 0;
    while let Some(reply) = next_of(&mut streams).await {
        assert!(unavailable(&reply));
        failed += 1;
    }
    assert_eq!(failed, 1000);
    assert_eq!(rig.core.queue.held(), 0);
    assert_eq!(rig.core.queue.expired_total(), 1000);
    assert_eq!(rig.a.state.dispatches(), 0);
    assert!(
        rig.core.slots.holding(),
        "an expiry does not release the gate"
    );
    until(|| rig.core.slots.open_streams() == 0).await;
    drop(guard);
}

#[tokio::test(start_paused = true)]
async fn the_sweep_fails_each_stream_once_and_a_late_release_cannot_bind_it() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let mut streams = JoinSet::new();
    for _ in 0..5 {
        let (response, _body) = client.start(CAPS, true).await;
        streams.spawn(finish(response));
    }
    until(|| rig.core.queue.held() == 5).await;
    tokio::time::advance(HOLD - Duration::from_secs(1)).await;
    assert_eq!(
        rig.core.queue.expire(crate::core::now()),
        0,
        "not yet past the bound"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    // Before any stream task has run: the sweep fails all five and counts them.
    assert_eq!(rig.core.queue.expire(crate::core::now()), 5);
    // A release now cannot bind a stream the sweep already failed.
    rig.core.slots.release_into(Slot::A).expect("A is open");
    while let Some(reply) = next_of(&mut streams).await {
        assert!(unavailable(&reply));
    }
    assert_eq!(
        rig.core.queue.expired_total(),
        5,
        "counted once, not again by the streams"
    );
    assert_eq!(rig.a.state.dispatches(), 0);
    drop(guard);
}

#[tokio::test(start_paused = true)]
async fn a_release_binds_every_held_stream_into_the_slot_it_names() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    rig.core.slots.open(Slot::B);
    let mut client = rig.client().await;
    let mut streams = JoinSet::new();
    for _ in 0..30 {
        let (response, mut body) = client.start(CAPS, false).await;
        body.send_data(Bytes::from_static(b"x"), true)
            .expect("body");
        streams.spawn(finish(response));
    }
    until(|| rig.core.queue.held() == 30).await;
    // A bypass stream is not held.
    assert_eq!(client.plain("GET", "/healthz", b"").await.text(), "ok");
    rig.core.slots.release_into(Slot::B).expect("B is open");
    drop(guard);
    while let Some(reply) = next_of(&mut streams).await {
        assert_eq!(reply.text(), "b");
    }
    assert_eq!(rig.b.state.dispatches(), 30);
    assert_eq!(rig.a.state.dispatches(), 0);
    assert_eq!(rig.core.queue.held(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_guard_fails_held_streams_at_once_and_later_ones_too() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let (response, _body) = client.start(CAPS, true).await;
    let held = tokio::spawn(finish(response));
    until(|| rig.core.queue.held() == 1).await;
    let before = tokio::time::Instant::now();
    drop(guard);
    assert!(unavailable(&held.await.expect("client")));
    assert!(
        before.elapsed() < Duration::from_secs(1),
        "at once, not at the bound"
    );
    assert_eq!(rig.core.slots.abandoned_holds(), 1);
    assert!(
        unavailable(&client.unary(CAPS).await),
        "no slot is active now"
    );
    assert_eq!(rig.a.state.dispatches(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_release_into_a_stopped_child_is_refused_and_binds_nothing() {
    let mut rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let (response, _body) = client.start(CAPS, true).await;
    let held = tokio::spawn(finish(response));
    until(|| rig.core.queue.held() == 1).await;
    // The child stopped and nothing replaces it: clear the active slot, retire the old one.
    rig.a.stop_accepting().await;
    rig.core.slots.clear_active();
    assert_eq!(rig.core.slots.retire(Slot::A), crate::slot::Retire::Closed);
    assert!(
        rig.core.slots.release_into(Slot::A).is_err(),
        "a closed slot is refused"
    );
    assert!(rig.core.slots.holding(), "the hold stays in force");
    drop(guard);
    assert!(unavailable(&held.await.expect("client")));
    assert_eq!(
        rig.core.metrics.upstream_dials.load(Ordering::Acquire),
        0,
        "no stream was bound to a dead child to wait out a connect",
    );
}

#[tokio::test(start_paused = true)]
async fn a_full_queue_refuses_at_once() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut streams = JoinSet::new();
    let mut clients = Vec::new();
    // 2048 held streams take three connections (1000 streams each at most).
    for count in [683, 683, 682] {
        let mut client = rig.client().await;
        for _ in 0..count {
            let (response, _body) = client.start(CAPS, true).await;
            streams.spawn(finish(response));
        }
        clients.push(client);
    }
    until(|| rig.core.queue.held() == 2048).await;
    let (response, _body) = clients[0].start(CAPS, true).await;
    let refused = finish(response).await;
    assert!(unavailable(&refused), "the 2049th is answered at once");
    assert_eq!(rig.core.metrics.hold_rejected.load(Ordering::Acquire), 1);
    assert_eq!(rig.core.queue.held(), 2048);
    tokio::time::advance(HOLD).await;
    while let Some(reply) = next_of(&mut streams).await {
        assert!(unavailable(&reply));
    }
    drop(guard);
}

/// Sends from `body` until the peer's window is gone and returns how many bytes it took.
async fn send_until_blocked(body: &mut h2::SendStream<Bytes>, want: usize) -> usize {
    body.reserve_capacity(want);
    let mut sent = 0;
    loop {
        let granted = tokio::time::timeout(
            Duration::from_secs(1),
            std::future::poll_fn(|cx| match body.poll_capacity(cx) {
                Poll::Ready(Some(Ok(n))) => Poll::Ready(n),
                Poll::Ready(_) => Poll::Ready(0),
                Poll::Pending => Poll::Pending,
            }),
        )
        .await;
        match granted {
            Ok(n) if n > 0 => {
                body.send_data(Bytes::from(vec![7_u8; n]), false)
                    .expect("data");
                sent += n;
            }
            _ => return sent,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn held_stream_credit_stops_at_window() {
    let received = Arc::new(AtomicUsize::new(0));
    let a = {
        let received = Arc::clone(&received);
        script(move |mut call| {
            let received = Arc::clone(&received);
            async move {
                let all = read_all(&mut call.body).await;
                received.store(all.len(), Ordering::Release);
                grpc_ok(&mut call.respond, "a");
            }
        })
    };
    let rig = Rig::start(a, tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    let (response, mut body) = client.start(CAPS, false).await;
    let sent = send_until_blocked(&mut body, 1 << 20).await;
    assert_eq!(
        sent, STREAM_WINDOW_BYTES as usize,
        "a held stream gets one window of credit and no more"
    );
    assert_eq!(rig.a.state.dispatches(), 0);

    // The release binds it: credit flows again and the rest of the body arrives.
    rig.core.slots.release_into(Slot::A).expect("A is open");
    drop(guard);
    let mut total = sent;
    while total < (1 << 20) {
        let more = send_until_blocked(&mut body, (1 << 20) - total).await;
        assert!(more > 0, "credit flows after the release");
        total += more;
    }
    body.send_data(Bytes::new(), true).expect("end");
    let reply = finish(response).await;
    assert_eq!(reply.text(), "a");
    assert_eq!(received.load(Ordering::Acquire), 1 << 20);
}

#[tokio::test(start_paused = true)]
async fn held_bodies_do_not_starve_a_bypass_request() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let guard = rig.core.slots.hold(HOLD).expect("no hold in force");
    let mut client = rig.client().await;
    // 999 held streams, each with a full window of body, and the update POST is the 1000th
    // stream on the connection: the most one connection carries.
    let window = Bytes::from(vec![1_u8; STREAM_WINDOW_BYTES as usize]);
    let mut held = Vec::new();
    for _ in 0..999 {
        let (response, mut body) = client.start(CAPS, false).await;
        body.reserve_capacity(window.len());
        let granted = std::future::poll_fn(|cx| body.poll_capacity(cx)).await;
        assert_eq!(granted.expect("capacity").expect("ok"), window.len());
        body.send_data(window.clone(), false)
            .expect("a full window");
        held.push((response, body));
    }
    until(|| rig.core.queue.held() == 999).await;
    let post = tokio::time::timeout(
        Duration::from_secs(5),
        client.plain("POST", "/v1/update/frontend", b"{}"),
    )
    .await
    .expect("the update POST completes with 999 full-window bodies held");
    assert_eq!(post.status, Some(503), "the built-in handler answered");
    tokio::time::advance(HOLD).await;
    for (response, body) in held {
        assert!(unavailable(&finish(response).await));
        drop(body);
    }
    drop(guard);
}

#[tokio::test(start_paused = true)]
async fn a_stream_refused_at_the_stream_limit_is_answered_at_once() {
    let limits = crate::config::Limits {
        max_streams: 1,
        ..crate::config::Limits::default()
    };
    let rig = Rig::start_with(
        script(|mut call| async move {
            read_all(&mut call.body).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
            grpc_ok(&mut call.respond, "late");
        }),
        tagged("b"),
        crate::control::NoUpdates,
        limits,
    )
    .await;
    let mut client = rig.client().await;
    let (first, mut body) = client.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let first = tokio::spawn(finish(first));
    until(|| rig.a.state.dispatches() == 1).await;
    let second = client.unary(CAPS).await;
    assert!(unavailable(&second));
    assert_eq!(rig.core.metrics.streams_refused.load(Ordering::Acquire), 1);
    assert_eq!(first.await.expect("client").text(), "late");
    // The count is released with the first stream: a third one is served.
    until(|| rig.core.slots.open_streams() == 0).await;
    assert_eq!(client.unary(CAPS).await.text(), "late");
}
