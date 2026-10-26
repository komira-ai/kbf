//! A request body that stalls or trickles, over a connection, run on the paused clock over
//! in-memory pipes: the server answers a body that sends no byte for the idle time and closes the
//! connection, stores nothing of it, and serves a body that is slow but steady. No test here
//! measures time: the waits are counted and the answers are awaited.

use std::time::Duration;

use hyper::StatusCode;

use crate::Backend;

use super::ops_tests::{completion, start};
use super::pipes::{Stalled, attach, status_of, until};
use super::testing::{BUCKET, Rig};

/// The idle time of the servers of these tests.
const IDLE: Duration = Duration::from_secs(1);

/// The wait between the bytes of a body that is slow but steady: shorter than the idle time.
const GAP: Duration = Duration::from_millis(500);

const _: () = assert!(GAP.as_millis() < IDLE.as_millis());

const _: () = assert!(GAP.as_millis() * 10 > IDLE.as_millis());

/// The body of the puts of these tests.
const BODY: &[u8] = b"0123456789";

/// A server whose idle time is `IDLE`.
async fn idle_rig() -> Rig {
    let started = Rig::with(64 << 20, |config| config.body_idle_timeout = IDLE);
    started.await
}

/// Asserts that a client that sends the head of a put and half its body and then stalls is
/// answered 400 `RequestTimeout` with `Connection: close`, that its connection is closed, that no
/// request is left in flight, and that nothing is stored: the client sends nothing more, so the
/// idle time is what brings the answer.
#[tokio::test(start_paused = true)]
async fn a_client_that_stalls_in_the_middle_of_a_body_is_answered_and_closed() {
    let rig = idle_rig().await;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = attach(&rig.server);
    let stalled = Stalled::open(client, &rig, &uri, &create, BODY, 4).await;
    let answer = stalled.answer().await;
    assert_eq!(status_of(&answer), 400);
    assert!(answer.contains("<Code>RequestTimeout</Code>"), "{answer}");
    assert!(answer.contains("connection: close"), "{answer}");
    until(&rig.server, |server| server.open_connections() == 0).await;
    assert_eq!(rig.server.in_flight(), 0);
    assert!(rig.hot_mem.get("k", None).await.is_err());
    assert!(rig.cold_mem.get("k", None).await.is_err());
}

/// Asserts that a body that arrives a byte at a time is stored: each wait for a byte is half the
/// idle time and there are ten of them, so the whole is five idle times and no wait for a byte is
/// one, and the put is answered 200 and the object is there. The waits are counted, not timed.
#[tokio::test(start_paused = true)]
async fn a_body_slower_than_the_idle_time_in_all_but_not_in_a_piece_is_stored() {
    let rig = idle_rig().await;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = attach(&rig.server);
    let stalled = Stalled::open(client, &rig, &uri, &create, BODY, 0).await;
    let (answer, waits) = stalled.trickle(GAP).await;
    assert_eq!(status_of(&answer), 200);
    assert_eq!(waits, 10);
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), "0123456789");
}

/// Asserts that a part whose body stalls is answered 400 `RequestTimeout` and is not stored: a
/// completion that names it is refused as `InvalidPart`.
#[tokio::test(start_paused = true)]
async fn a_part_that_stalls_is_refused_and_not_stored() {
    let rig = idle_rig().await;
    let id = start(&rig, "big").await;
    let uri = format!("/{BUCKET}/big?partNumber=1&uploadId={id}");
    let client = attach(&rig.server);
    let stalled = Stalled::open(client, &rig, &uri, &[], BODY, 4).await;
    let answer = stalled.answer().await;
    assert_eq!(status_of(&answer), 400);
    assert!(answer.contains("<Code>RequestTimeout</Code>"), "{answer}");
    let body = completion(&[(1, "\"x\"")]);
    let finish = format!("big?uploadId={id}");
    let done = rig.call("POST", &finish, body.as_bytes()).await;
    assert_eq!(done.status, StatusCode::BAD_REQUEST);
    assert_eq!(done.code(), "InvalidPart");
}
