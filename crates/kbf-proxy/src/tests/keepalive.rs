//! PING keepalive, both sides: a dead peer is closed and nothing else, an upstream that stops
//! answering is avoided and only closed at the backstop.

use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::time::Instant;

use crate::config::Limits;
use crate::control::NoUpdates;

use super::kit::{
    CAPS, EXECUTE, RawFrame, RawPeer, Rig, finish, grpc_ok, read_all, script, tagged,
};

const MINUTE: Duration = Duration::from_secs(60);

/// Catches a keepalive that closes a live connection, or a stream on one, and one that never
/// closes a peer that does not answer. A client that is silent when pinged is closed after
/// the interval and the ack bound (30 s and 20 s); a live client on another connection, with
/// an Execute that has said nothing for ten minutes, is untouched and gets its answer.
#[tokio::test(start_paused = true)]
async fn keepalive_closes_dead_peer_only() {
    let slow = script(|mut call| async move {
        read_all(&mut call.body).await;
        tokio::time::sleep(10 * MINUTE).await;
        grpc_ok(&mut call.respond, "late");
    });
    let rig = Rig::start(slow, tagged("b")).await;
    let mut live = rig.client().await;
    let (response, mut body) = live.start(EXECUTE, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    let live_reply = tokio::spawn(finish(response));

    let start = Instant::now();
    let mut dead = RawPeer::connect(&rig).await;
    let mut pings = 0;
    // Bounded, so a keepalive that never closes it fails here and does not hang the test.
    let closed = tokio::time::timeout(2 * MINUTE, async {
        while let Some(frame) = dead.next().await {
            if frame.kind == RawFrame::PING && frame.flags & 1 == 0 {
                pings += 1;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the silent peer was never closed");
    assert_eq!(pings, 1, "one PING, and no answer");
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_secs(50) && waited < Duration::from_secs(55),
        "closed 30 s of silence plus 20 s for the ack after it connected: {waited:?}"
    );

    assert!(live.alive(), "the live connection was not closed");
    let reply = live_reply.await.expect("client");
    assert!(reply.error.is_none(), "{:?}", reply.error);
    assert_eq!(reply.text(), "late", "the idle Execute got its answer");
    assert_eq!(reply.grpc_status().as_deref(), Some("0"));
    assert!(start.elapsed() >= 10 * MINUTE);
}

/// Catches intervals that are fixed in code and not read from the limits: with a 5 s interval
/// and a 3 s ack, a silent peer is closed 8 s after it connected.
#[tokio::test(start_paused = true)]
async fn the_downstream_keepalive_follows_the_configured_limits() {
    let limits = Limits {
        keepalive_interval_s: 5,
        keepalive_ack_s: 3,
        ..Limits::default()
    };
    let rig = Rig::start_with(tagged("a"), tagged("b"), NoUpdates, limits).await;
    let start = Instant::now();
    let mut dead = RawPeer::connect(&rig).await;
    let closed = tokio::time::timeout(MINUTE, async { while dead.next().await.is_some() {} }).await;
    assert!(closed.is_ok(), "the silent peer was never closed");
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_secs(8) && waited < Duration::from_secs(9),
        "{waited:?}"
    );
}

/// Catches an upstream keepalive that never pings (a quiet loopback connection goes cold) and
/// one that opens a new connection for each stream because it thinks the quiet one is gone.
#[tokio::test(start_paused = true)]
async fn upstream_ping_keeps_a_quiet_connection_warm() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    *rig.a.state.raw_pongs_first.lock().expect("lock") = Some(usize::MAX);
    let mut client = rig.client().await;
    // Answered by nobody: the stream and its connection just sit there.
    let (_first, _body) = client.start(CAPS, true).await;
    tokio::time::sleep(Duration::from_secs(95)).await;
    // The dial check, then 30, 60 and 90 s.
    assert_eq!(rig.a.state.pings.load(Ordering::Acquire), 4);
    let (_second, _body) = client.start(CAPS, true).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let dials = rig.core.metrics.upstream_dials.load(Ordering::Acquire);
    assert_eq!(dials, 1, "the second stream used the warm connection");
}

/// Catches a pool that keeps putting streams on a connection whose child has stopped
/// answering PINGs (the stream would wait for a child that is not there), and an upstream
/// keepalive that closes at the ack bound instead of the backstop: a quiesce can stall a
/// loopback child for longer than 20 s, and the streams already on it are still good.
#[tokio::test(start_paused = true)]
async fn an_unanswered_upstream_ping_stops_new_streams_and_only_the_backstop_closes() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    // The dial check is answered; nothing after it is.
    *rig.a.state.raw_pongs_first.lock().expect("lock") = Some(1);
    let mut client = rig.client().await;
    let start = Instant::now();
    let (response, _body) = client.start(CAPS, true).await;
    let first = tokio::spawn(finish(response));
    tokio::time::sleep(Duration::from_secs(55)).await;
    assert_eq!(rig.a.state.pings.load(Ordering::Acquire), 2);

    // The PING of 30 s is unanswered at 50 s: a new stream goes to a new connection.
    let second = tokio::time::timeout(Duration::from_secs(5), client.unary(CAPS))
        .await
        .expect("the stream was put on the suspect connection");
    assert_eq!(second.text(), "a");
    assert_eq!(rig.core.metrics.upstream_dials.load(Ordering::Acquire), 2);

    // The first stream is still there at 5 minutes, and gone with the connection at the
    // backstop: 300 s after the PING.
    tokio::time::sleep(Duration::from_secs(260)).await;
    assert!(!first.is_finished(), "closed before the backstop");
    let reply = first.await.expect("client");
    assert_eq!(reply.grpc_status().as_deref(), Some("14"));
    let waited = start.elapsed();
    assert!(
        waited >= Duration::from_secs(330) && waited < Duration::from_secs(335),
        "{waited:?}"
    );
}
