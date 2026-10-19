//! The h2 flood limits: a burst of client cancels that the 1000-stream cap allows trips
//! nothing, and a real rapid-reset attack is answered with a GOAWAY on its own connection.

use std::time::Duration;

use bytes::Bytes;
use h2::Reason;

use super::kit::{CAPS, RawFrame, RawPeer, Rig, finish, frame_on, grpc_ok, script, tagged, until};
use crate::limits::{DOWNSTREAM_CONNECTION_STREAMS, PENDING_RESET_STREAMS};

/// `ENHANCE_YOUR_CALM`, the error code h2 puts on the GOAWAY of a flood.
const ENHANCE_YOUR_CALM: u32 = 0xb;

/// `n` streams, each opened and reset at once, written as one burst.
fn pairs(n: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| {
            let id = u32::try_from(2 * i + 1).expect("a small id");
            [RawPeer::request(id), RawPeer::cancel(id)].concat()
        })
        .collect()
}

/// Reads until the proxy answers `stream` with its HEADERS; a GOAWAY first is a failure.
async fn answered(peer: &mut RawPeer, stream: u32) {
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = peer.next().await {
            assert_ne!(frame.kind, RawFrame::GOAWAY, "the connection was sent away");
            if frame.kind == RawFrame::HEADERS && frame.stream == stream {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(seen, Ok(true), "the connection stopped serving");
}

/// Catches the 1000 streams of a Buck2 build, all open on one connection, being cancelled in
/// one go (the shape of a build cancel): every cancel reaches the child and frees its permit,
/// the connection is not sent away and serves the next call.
#[tokio::test(start_paused = true)]
async fn a_thousand_client_cancels_on_one_connection_trip_nothing() {
    let streams = DOWNSTREAM_CONNECTION_STREAMS as usize;
    let a = script(move |call| async move {
        if call.nth <= streams {
            // Held until the proxy resets it.
            let _call = call;
            std::future::pending::<()>().await;
        } else {
            let mut call = call;
            grpc_ok(&mut call.respond, "after");
        }
    });
    let rig = Rig::start(a, tagged("b")).await;
    let mut client = rig.client().await;
    let mut open = Vec::new();
    for _ in 0..streams {
        open.push(client.start(CAPS, false).await);
    }
    until(|| rig.a.state.dispatches() == streams).await;
    assert_eq!(
        rig.core.slots.open_permits(crate::slot::Slot::A),
        streams as u64
    );
    for (_response, body) in &mut open {
        body.send_reset(Reason::CANCEL);
    }
    until(|| rig.core.slots.open_permits(crate::slot::Slot::A) == 0).await;
    assert!(client.alive(), "the connection was not sent away");
    assert_eq!(client.unary(CAPS).await.text(), "after");
}

/// Catches the h2 default of about 20 reset streams that are not yet accepted: a client that
/// opens and cancels a thousand streams before the proxy has read the first of them (one
/// burst, as a build cancel over a busy connection can be) is within the limit, so the
/// connection keeps serving. At the default it is sent a GOAWAY.
#[tokio::test(start_paused = true)]
async fn a_thousand_streams_opened_and_cancelled_in_one_burst_trip_nothing() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut peer = RawPeer::connect(&rig).await;
    // One more than the stream cap: h2 trips on the reset after its limit, so a limit of
    // exactly 1000 passes 1000 pairs and fails this.
    let n = DOWNSTREAM_CONNECTION_STREAMS as usize + 1;
    peer.send(&pairs(n)).await;
    let probe = u32::try_from(2 * n + 1).expect("a small id");
    peer.send(&RawPeer::request(probe)).await;
    answered(&mut peer, probe).await;
}

/// Catches a rapid reset that is not stopped, and a stop that reaches other connections: a
/// peer that opens and resets streams past the limit is sent `ENHANCE_YOUR_CALM` and closed,
/// while a client on another connection is served before and after.
#[tokio::test(start_paused = true)]
async fn a_rapid_reset_attack_is_sent_away_on_its_own_connection_only() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut good = rig.client().await;
    assert_eq!(good.unary(CAPS).await.text(), "a");

    let mut attacker = RawPeer::connect(&rig).await;
    attacker.send(&pairs(PENDING_RESET_STREAMS + 500)).await;
    let sent_away = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = attacker.next().await {
            if frame.kind == RawFrame::GOAWAY {
                return Some(frame.goaway_code());
            }
        }
        None
    })
    .await;
    assert_eq!(sent_away, Ok(Some(ENHANCE_YOUR_CALM)));
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        while attacker.next().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "the connection was left open");

    assert!(good.alive());
    let (response, mut body) = good.start(CAPS, false).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    assert_eq!(finish(response).await.text(), "a");
}

/// Waits for the proxy to send `GOAWAY` and close the connection.
async fn sent_away(peer: &mut RawPeer) -> bool {
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        let mut goaway = false;
        while let Some(frame) = peer.next().await {
            goaway |= frame.kind == RawFrame::GOAWAY;
        }
        goaway
    })
    .await;
    seen == Ok(true)
}

/// Catches a CONTINUATION flood: a header block that never ends (HEADERS and then frames
/// without END_HEADERS) is cut off with a GOAWAY on its own connection after a few frames,
/// and does not hold memory for as long as the peer keeps sending. A client on another
/// connection is untouched.
#[tokio::test(start_paused = true)]
async fn a_continuation_flood_is_sent_away_on_its_own_connection_only() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut good = rig.client().await;
    let mut attacker = RawPeer::connect(&rig).await;
    let mut flood = frame_on(RawFrame::HEADERS, 0, 1, &[0x83, 0x86, 0x84]);
    for _ in 0..16 {
        flood.extend(frame_on(RawFrame::CONTINUATION, 0, 1, &[0x00]));
    }
    attacker.send(&flood).await;
    assert!(sent_away(&mut attacker).await, "the flood was not cut off");
    assert_eq!(good.unary(CAPS).await.text(), "a");
}

/// Catches a header list cap that is not the configured one: a request whose headers decode to
/// more than `max_header_list_bytes` is answered on its own stream and never reaches a child,
/// the connection is not sent away, and the next call on it is served.
#[tokio::test(start_paused = true)]
async fn an_oversize_header_list_is_refused_on_its_stream_only() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut peer = RawPeer::connect(&rig).await;
    // `x: <20000 bytes>` as a literal field without indexing (name and value literal), so the
    // block is over the 16 KiB cap and spans a HEADERS and a CONTINUATION frame.
    let mut block = RawPeer::grpc_block(CAPS);
    block.extend([0x00, 1, b'x', 0x7f, 0xa1, 0x9b, 0x01]);
    block.extend(std::iter::repeat_n(b'v', 20_000));
    let (first, rest) = block.split_at(16 * 1024);
    let mut bytes = frame_on(RawFrame::HEADERS, RawFrame::END_STREAM, 1, first);
    bytes.extend(frame_on(RawFrame::CONTINUATION, 4, 1, rest));
    bytes.extend(RawPeer::grpc_request(3, CAPS));
    peer.send(&bytes).await;
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        let mut refused = false;
        while let Some(frame) = peer.next().await {
            assert_ne!(frame.kind, RawFrame::GOAWAY, "the connection was sent away");
            refused |= frame.stream == 1
                && (frame.kind == RawFrame::RST_STREAM || frame.kind == RawFrame::HEADERS);
            if frame.kind == RawFrame::HEADERS && frame.stream == 3 {
                return refused;
            }
        }
        false
    })
    .await;
    assert_eq!(
        seen,
        Ok(true),
        "stream 1 was not refused, or stream 3 not served"
    );
    assert_eq!(
        rig.a.state.dispatches(),
        1,
        "only the call of stream 3 reached the child"
    );
}
