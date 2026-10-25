//! The idle timeout of bodies and answers over real connections: a client that stalls in the
//! middle of a body, one that is slow but steady, one that stops reading an answer, and a drain
//! that meets them.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tierstore::Backend;
use tierstore::server::Drained;
use tokio::io::AsyncReadExt;

use crate::drain::{put_headers, until_in_flight};
use crate::raw::call;
use crate::stalled::{Stalled, read_answer, send_get, status_of};
use crate::support::{BUCKET, Harness};

/// The idle timeout of the servers of these tests.
const IDLE: Duration = Duration::from_secs(1);

/// How long past the idle timeout a stalled client may take to be answered or closed.
const SLACK: Duration = Duration::from_secs(5);

/// The size of an object that the socket buffers cannot hold, so that a client that does not read
/// holds its answer back.
const BIG: usize = 48 << 20;

/// A server with the idle timeout of these tests, and a drain that would wait long.
async fn start() -> Harness {
    Harness::start_fixed(|config| {
        config.body_idle_timeout = IDLE;
        config.drain_lb_wait = Duration::from_millis(20);
        config.drain_deadline = Duration::from_secs(30);
    })
    .await
}

/// Puts the object `big`, of `BIG` bytes, in the hot tier.
async fn put_big(h: &Harness) {
    let object = Bytes::from(vec![7_u8; BIG]);
    h.hot_mem.put("big", object).await.unwrap();
}

/// Waits until the server has no connection open.
async fn until_closed(h: &Harness) {
    let waiting = async {
        while h.server.open_connections() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let waited = tokio::time::timeout(SLACK, waiting).await;
    assert!(waited.is_ok());
}

/// The id in the answer to a start of a multipart upload.
pub fn upload_id(xml: &str) -> &str {
    let (_, rest) = xml.split_once("<UploadId>").unwrap();
    rest.split_once("</UploadId>").unwrap().0
}

/// Asserts that a client that sends the head of a put and half its body and then stalls is
/// answered 400 `RequestTimeout` with `Connection: close` once the idle time has passed, and its
/// connection is closed, all within the idle time and a slack, and that nothing is stored.
#[tokio::test]
async fn a_client_that_stalls_in_the_middle_of_a_body_is_answered_and_closed() {
    let h = start().await;
    let body = b"0123456789";
    let (headers, path) = put_headers(&h, body);
    let stalled = Stalled::open(h.addr, &headers, &path, body, 4).await;
    let started = Instant::now();
    let waiting = tokio::time::timeout(SLACK, stalled.answer());
    let answer = waiting.await.unwrap();
    let waited = started.elapsed();
    assert_eq!(status_of(&answer), 400);
    assert!(answer.contains("<Code>RequestTimeout</Code>"), "{answer}");
    assert!(answer.contains("connection: close"), "{answer}");
    assert!(waited >= IDLE * 2 / 3, "{waited:?}");
    until_closed(&h).await;
    assert_eq!(h.server.in_flight(), 0);
    assert!(h.hot_mem.get("k", None).await.is_err());
    assert!(h.cold_mem.get("k", None).await.is_err());
}

/// Asserts that a body that arrives a byte at a time is stored: the whole takes longer than the
/// idle time and no wait for a byte does, so the put is answered 200 and the object is there.
#[tokio::test]
async fn a_body_slower_than_the_idle_time_in_all_but_not_in_a_piece_is_stored() {
    let h = start().await;
    let body = b"0123456789";
    let (headers, path) = put_headers(&h, body);
    let stalled = Stalled::open(h.addr, &headers, &path, body, 0).await;
    let started = Instant::now();
    let status = stalled.trickle(Duration::from_millis(200)).await;
    assert_eq!(status, 200);
    assert!(started.elapsed() > IDLE, "{:?}", started.elapsed());
    assert_eq!(h.hot_mem.get("k", None).await.unwrap(), "0123456789");
}

/// Asserts that a part whose body stalls is answered 400 `RequestTimeout` and is not stored: a
/// completion that names it is refused as `InvalidPart`.
#[tokio::test]
async fn a_part_that_stalls_is_refused_and_not_stored() {
    let h = start().await;
    let uploads = [("uploads", "")];
    let started = call(&h, "POST", "big", &uploads, &[], b"").await;
    let xml = started.text().await.unwrap();
    let id = upload_id(&xml);
    let body = b"0123456789";
    let query = [("partNumber", "1"), ("uploadId", id)];
    let base = format!("/{BUCKET}/big");
    let signed = h.signed("PUT", &base, &query, &[], body);
    let path = format!("{base}?partNumber=1&uploadId={id}");
    let stalled = Stalled::open(h.addr, &signed, &path, body, 4).await;
    let waiting = tokio::time::timeout(SLACK, stalled.answer());
    let answer = waiting.await.unwrap();
    assert_eq!(status_of(&answer), 400);
    assert!(answer.contains("<Code>RequestTimeout</Code>"), "{answer}");
    let parts = "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"x\"</ETag>\
                 </Part></CompleteMultipartUpload>";
    let finish = [("uploadId", id)];
    let complete = parts.as_bytes();
    let done = call(&h, "POST", "big", &finish, &[], complete).await;
    assert_eq!(done.status(), 400);
    let text = done.text().await.unwrap();
    assert!(text.contains("<Code>InvalidPart</Code>"), "{text}");
}

/// Asserts that a get whose client stops reading is closed after the idle time: the connection is
/// open a third of the idle time after the answer began, the server has no connection open after
/// the idle time, and the client got a part of the answer and then the end of the connection.
#[tokio::test]
async fn a_get_whose_client_stops_reading_is_closed_after_the_idle_time() {
    let h = start().await;
    put_big(&h).await;
    let mut stream = send_get(&h, "big").await;
    tokio::time::sleep(IDLE / 3).await;
    assert_eq!(h.server.open_connections(), 1);
    until_closed(&h).await;
    let mut received = Vec::new();
    let _ = stream.read_to_end(&mut received).await;
    assert!(received.starts_with(b"HTTP/1.1 200"));
    assert!(received.len() < BIG);
}

/// Asserts that a drain does not wait past the idle time for a client that has stopped reading
/// its answer: it ends with nothing cut within a slack, though its deadline is thirty seconds.
#[tokio::test]
async fn a_drain_does_not_wait_past_the_idle_time_for_a_client_that_stopped_reading() {
    let h = start().await;
    put_big(&h).await;
    let stream = send_get(&h, "big").await;
    tokio::time::sleep(IDLE / 3).await;
    h.server.begin_drain();
    let drained = tokio::time::timeout(SLACK, h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
    drop(stream);
}

/// Asserts that a drain does not wait past the idle time for a client that stalled in the middle
/// of a body: the request ends with its timeout, answered 400, and the drain ends with nothing
/// cut within a slack, though its deadline is thirty seconds.
#[tokio::test]
async fn a_drain_does_not_wait_past_the_idle_time_for_a_client_that_stalled() {
    let h = start().await;
    let body = b"0123456789";
    let (headers, path) = put_headers(&h, body);
    let stalled = Stalled::open(h.addr, &headers, &path, body, 4).await;
    until_in_flight(&h).await;
    h.server.begin_drain();
    let drained = tokio::time::timeout(SLACK, h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
    let answer = stalled.answer().await;
    assert_eq!(status_of(&answer), 400);
    assert!(h.hot_mem.get("k", None).await.is_err());
}

/// Asserts that a client that reads its answer slowly but steadily is served: a get of an object
/// that the socket buffers cannot hold is read in pieces with waits shorter than the idle time,
/// and the whole takes longer than the idle time.
#[tokio::test]
async fn a_client_that_reads_slowly_but_steadily_gets_the_whole_answer() {
    let h = start().await;
    put_big(&h).await;
    let mut stream = send_get(&h, "big").await;
    let started = Instant::now();
    let mut total = 0;
    let mut buffer = vec![0_u8; 1 << 20];
    for _ in 0..6 {
        tokio::time::sleep(IDLE / 3).await;
        total += stream.read(&mut buffer).await.unwrap();
    }
    assert!(started.elapsed() > IDLE);
    total += read_answer(&mut stream).await.len();
    assert!(total > BIG);
}
