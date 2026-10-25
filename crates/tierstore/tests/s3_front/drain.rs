//! The drain: `/readyz` says 503, the server serves on through the wait, refuses new requests,
//! waits for those in flight up to the deadline, and ends.

use std::time::Duration;

use bytes::Bytes;
use reqwest::Client;
use tierstore::Backend;
use tierstore::server::Drained;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::raw::call;
use crate::stalled::{Stalled, head, read_answer, send_get, status_of};
use crate::support::{BUCKET, Harness};

/// Waits until the server has a request in flight.
pub async fn until_in_flight(h: &Harness) {
    let waiting = async {
        while h.server.in_flight() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let waited = tokio::time::timeout(Duration::from_secs(5), waiting).await;
    assert!(waited.is_ok());
}

/// The signed headers of a put of `body` to the key `k`, and its path.
pub fn put_headers(h: &Harness, body: &[u8]) -> (Vec<(String, String)>, String) {
    let path = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let headers = h.signed("PUT", &path, &[], &create, body);
    (headers, path)
}

/// Asserts the sequence of a drain: `/readyz` is 503 at once, the S3 calls are still served
/// through the wait, then, while a request in flight holds the drain open, a connection that is
/// open is answered 503 `ServiceUnavailable` with `Connection: close` and a new connection is
/// refused, and the drain ends with nothing cut once that request is done.
#[tokio::test]
async fn the_drain_serves_through_the_wait_then_refuses_then_ends() {
    let h = Harness::start_fixed(|config| {
        config.drain_lb_wait = Duration::from_millis(400);
        config.drain_deadline = Duration::from_secs(5);
    })
    .await;
    let ready = format!("http://{}/readyz", h.addr);
    let client = Client::new();
    assert_eq!(client.get(&ready).send().await.unwrap().status(), 200);
    let mut idle = TcpStream::connect(h.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    h.server.begin_drain();
    assert_eq!(client.get(&ready).send().await.unwrap().status(), 503);
    let create = [("if-none-match", "*")];
    let during = call(&h, "PUT", "k", &[], &create, b"served").await;
    assert_eq!(during.status(), 200);
    assert!(h.hot_mem.get("k", None).await.is_ok());
    let held_body = b"0123456789";
    let held_path = format!("/{BUCKET}/held");
    let held_headers = h.signed("PUT", &held_path, &[], &create, held_body);
    let held = Stalled::open(h.addr, &held_headers, &held_path, held_body, 4).await;
    until_in_flight(&h).await;

    tokio::time::sleep(Duration::from_millis(900)).await;
    let signed = h.signed("GET", "/tier-bucket/k", &[], &[], b"");
    let text = head("GET", "/tier-bucket/k", &signed, 0);
    idle.write_all(text.as_bytes()).await.unwrap();
    let reading = tokio::time::timeout(Duration::from_secs(5), read_answer(&mut idle));
    let answer = reading.await.unwrap();
    assert_eq!(status_of(&answer), 503);
    assert!(answer.contains("connection: close"), "{answer}");
    assert!(answer.contains("<Code>ServiceUnavailable</Code>"), "{answer}");
    assert!(TcpStream::connect(h.addr).await.is_err());
    assert!(!h.served.is_finished());

    assert_eq!(held.finish().await, 200);
    let drained = tokio::time::timeout(Duration::from_secs(5), h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
}

/// Asserts that a request in flight when the drain begins is allowed to finish: the drain does
/// not end while it is running, the request is answered as if there were no drain, and the
/// drain then ends with nothing cut.
#[tokio::test]
async fn a_request_in_flight_finishes_before_the_drain_ends() {
    let h = Harness::start_fixed(|config| {
        config.drain_lb_wait = Duration::from_millis(50);
        config.drain_deadline = Duration::from_secs(10);
    })
    .await;
    let body = b"0123456789";
    let (headers, path) = put_headers(&h, body);
    let stalled = Stalled::open(h.addr, &headers, &path, body, 4).await;
    until_in_flight(&h).await;
    h.server.begin_drain();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!h.served.is_finished());
    assert_eq!(h.server.in_flight(), 1);
    assert_eq!(stalled.finish().await, 200);
    let drained = tokio::time::timeout(Duration::from_secs(5), h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
    assert_eq!(h.hot_mem.get("k", None).await.unwrap(), "0123456789");
}

/// Asserts that the drain does not end while an answer is still being sent: the answer to a get
/// of a large object that the client has not begun to read is held back by the socket buffers
/// when the drain begins, the drain goes on past the wait, and it ends, with nothing cut, once
/// the client has read the whole answer.
#[tokio::test]
async fn the_drain_waits_for_an_answer_that_is_being_sent() {
    let h = Harness::start_fixed(|config| {
        config.drain_lb_wait = Duration::from_millis(50);
        config.drain_deadline = Duration::from_secs(20);
    })
    .await;
    let size = 48 << 20;
    let body = Bytes::from(vec![7_u8; size]);
    h.hot_mem.put("big", body).await.unwrap();
    let mut stream = send_get(&h, "big").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.server.begin_drain();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!h.served.is_finished());
    let answer = read_answer(&mut stream).await;
    assert_eq!(status_of(&answer), 200);
    assert!(answer.len() > size);
    let drained = tokio::time::timeout(Duration::from_secs(5), h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
}

/// Asserts that an answer still being sent at the deadline is counted as cut: the drain ends at
/// the deadline with one request cut when the client has not read the answer to a get.
#[tokio::test]
async fn an_answer_still_being_sent_at_the_deadline_is_counted_as_cut() {
    let h = Harness::start_fixed(|config| {
        config.drain_lb_wait = Duration::from_millis(20);
        config.drain_deadline = Duration::from_millis(300);
    })
    .await;
    let body = Bytes::from(vec![7_u8; 48 << 20]);
    h.hot_mem.put("big", body).await.unwrap();
    let stream = send_get(&h, "big").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.server.begin_drain();
    let drained = tokio::time::timeout(Duration::from_secs(5), h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 1 });
    drop(stream);
}

/// Asserts that a request still running at the deadline is counted as cut and the drain ends
/// without it; the request itself goes on.
#[tokio::test]
async fn a_request_still_running_at_the_deadline_is_counted_as_cut() {
    let h = Harness::start_fixed(|config| {
        config.drain_lb_wait = Duration::from_millis(20);
        config.drain_deadline = Duration::from_millis(200);
    })
    .await;
    let body = b"0123456789";
    let (headers, path) = put_headers(&h, body);
    let stalled = Stalled::open(h.addr, &headers, &path, body, 4).await;
    until_in_flight(&h).await;
    h.server.begin_drain();
    let drained = tokio::time::timeout(Duration::from_secs(5), h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 1 });
    assert_eq!(h.server.in_flight(), 1);
    assert_eq!(stalled.finish().await, 200);
    assert_eq!(h.server.in_flight(), 0);
}
