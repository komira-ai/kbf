//! The drain, run on the paused clock over in-memory pipes: the accept loop takes its connections
//! from a channel, and each test counts what happens (requests in flight, connections open, the
//! requests the deadline cut) and awaits it. No test here measures time, and none waits on a
//! real clock.

use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::Backend;

use super::pipes::{
    OBJECT, Serving, Stalled, begin_get, call, drained, head, probe, put_big, read_answer,
    read_rest, read_rest_slowly, status_of, until,
};
use super::testing::BUCKET;
use super::{Drained, ServerConfig};

/// The body of the puts of these tests, of which the first four bytes are sent.
const BODY: &[u8] = b"0123456789";

/// Half of the idle time that `idle_one_second` sets.
const HALF_IDLE: Duration = Duration::from_millis(500);

/// A drain that waits ten seconds, then as long as the requests need (an hour at most), over
/// bodies and answers that may idle for an hour.
fn patient(config: &mut ServerConfig) {
    config.drain_lb_wait = Duration::from_secs(10);
    config.drain_deadline = Duration::from_secs(3600);
    config.body_idle_timeout = Duration::from_secs(3600);
}

/// An idle time of one second, a drain that waits a moment, and a deadline of thirty seconds.
fn idle_one_second(config: &mut ServerConfig) {
    config.body_idle_timeout = Duration::from_secs(1);
    config.drain_lb_wait = Duration::from_millis(20);
    config.drain_deadline = Duration::from_secs(30);
}

/// Asserts the sequence of a drain: `/readyz` is 503 at once, the S3 calls are still served
/// through the wait, then, while a request in flight holds the drain open, a connection that is
/// open is answered 503 `ServiceUnavailable` with `Connection: close` and a new connection is
/// refused, and the drain ends with nothing cut once that request is done.
#[tokio::test(start_paused = true)]
async fn the_drain_serves_through_the_wait_then_refuses_then_ends() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let ready = probe(&serving, "/readyz").await;
    assert_eq!(status_of(&ready), 200);
    until(&rig.server, |server| server.open_connections() == 0).await;
    let mut idle = serving.connect().unwrap();
    until(&rig.server, |server| server.open_connections() == 1).await;

    rig.server.begin_drain();
    let draining = probe(&serving, "/readyz").await;
    assert_eq!(status_of(&draining), 503);
    let create = [("if-none-match", "*")];
    let uri = format!("/{BUCKET}/k");
    let during = call(&serving, "PUT", &uri, &create, b"served").await;
    assert_eq!(status_of(&during), 200);
    assert!(rig.hot_mem.get("k", None).await.is_ok());
    let held_uri = format!("/{BUCKET}/held");
    let client = serving.connect().unwrap();
    let held = Stalled::open(client, rig, &held_uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;

    serving.refusing().await;
    let text = head(rig, "GET", &uri, &[], b"", "");
    idle.write_all(text.as_bytes()).await.unwrap();
    let refused = read_answer(&mut idle).await;
    assert_eq!(status_of(&refused), 503);
    assert!(refused.contains("connection: close"), "{refused}");
    assert!(
        refused.contains("<Code>ServiceUnavailable</Code>"),
        "{refused}"
    );
    assert!(serving.connect().is_none());
    assert!(!serving.served.is_finished());

    let finished = held.finish().await;
    assert_eq!(status_of(&finished), 200);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a request in flight when the drain begins is allowed to finish: the drain does
/// not end while it is running, the request is answered as if there were no drain, and the
/// drain then ends with nothing cut.
#[tokio::test(start_paused = true)]
async fn a_request_in_flight_finishes_before_the_drain_ends() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    until(&rig.server, |server| server.in_flight() == 0).await;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = serving.connect().unwrap();
    let held = Stalled::open(client, rig, &uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;
    rig.server.begin_drain();
    serving.refusing().await;
    assert!(!serving.served.is_finished());
    assert_eq!(rig.server.in_flight(), 1);
    let answer = held.finish().await;
    assert_eq!(status_of(&answer), 200);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), "0123456789");
}

/// Asserts that the drain does not end while an answer is still being sent: the answer to a get
/// of a large object that the client has not read past its first piece is held back by the pipe
/// when the drain begins, the drain goes on past the wait, and it ends, with nothing cut, once
/// the client has read the whole answer.
#[tokio::test(start_paused = true)]
async fn the_drain_waits_for_an_answer_that_is_being_sent() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    put_big(rig).await;
    let mut client = serving.connect().unwrap();
    let mut answer = begin_get(rig, &mut client, "big").await;
    rig.server.begin_drain();
    serving.refusing().await;
    assert!(!serving.served.is_finished());
    read_rest(&mut client, &mut answer).await;
    assert!(answer.starts_with(b"HTTP/1.1 200"));
    assert!(answer.len() > OBJECT);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that an answer still being sent at the deadline is counted as cut: the drain ends at
/// the deadline with one request cut when the client has not read the answer to a get, and that
/// connection is still open then.
#[tokio::test(start_paused = true)]
async fn an_answer_still_being_sent_at_the_deadline_is_counted_as_cut() {
    let serving = Serving::start(|config| {
        config.drain_lb_wait = Duration::from_millis(20);
        config.drain_deadline = Duration::from_millis(300);
    })
    .await;
    let rig = &serving.rig;
    put_big(rig).await;
    let mut client = serving.connect().unwrap();
    let first = begin_get(rig, &mut client, "big").await;
    assert!(first.starts_with(b"HTTP/1.1 200"));
    rig.server.begin_drain();
    assert_eq!(drained(serving.served).await, Drained { abandoned: 1 });
    assert_eq!(rig.server.open_connections(), 1);
}

/// Asserts that a request still running at the deadline is counted as cut and the drain ends
/// without it; the request itself goes on.
#[tokio::test(start_paused = true)]
async fn a_request_still_running_at_the_deadline_is_counted_as_cut() {
    let serving = Serving::start(|config| {
        config.drain_lb_wait = Duration::from_millis(20);
        config.drain_deadline = Duration::from_millis(200);
    })
    .await;
    let rig = &serving.rig;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = serving.connect().unwrap();
    let held = Stalled::open(client, rig, &uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;
    rig.server.begin_drain();
    assert_eq!(drained(serving.served).await, Drained { abandoned: 1 });
    assert_eq!(rig.server.in_flight(), 1);
    let answer = held.finish().await;
    assert_eq!(status_of(&answer), 200);
    assert_eq!(rig.server.in_flight(), 0);
}

/// Asserts that a drain does not wait past the idle time for a client that has stopped reading
/// its answer: it ends with nothing cut, though its deadline is thirty seconds, and the client got
/// a part of the answer and then the end of the connection.
#[tokio::test(start_paused = true)]
async fn a_drain_does_not_wait_past_the_idle_time_for_a_client_that_stopped_reading() {
    let serving = Serving::start(idle_one_second).await;
    let rig = &serving.rig;
    put_big(rig).await;
    let mut client = serving.connect().unwrap();
    let mut answer = begin_get(rig, &mut client, "big").await;
    rig.server.begin_drain();
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    assert_eq!(rig.server.open_connections(), 0);
    read_rest(&mut client, &mut answer).await;
    assert!(answer.starts_with(b"HTTP/1.1 200"));
    assert!(answer.len() < OBJECT);
}

/// Asserts that a server answering a client that reads slowly but steadily does not cut it: the
/// client reads one piece of the answer every half of the idle time, over more than two reads (they
/// are counted, not timed), and gets the whole answer and the end of the connection, and the drain
/// that follows ends with nothing cut.
#[tokio::test(start_paused = true)]
async fn a_client_that_reads_slowly_but_steadily_gets_the_whole_answer() {
    let serving = Serving::start(idle_one_second).await;
    let rig = &serving.rig;
    put_big(rig).await;
    let mut client = serving.connect().unwrap();
    let mut answer = begin_get(rig, &mut client, "big").await;
    let reads = read_rest_slowly(&mut client, &mut answer, HALF_IDLE).await;
    assert!(answer.starts_with(b"HTTP/1.1 200"));
    assert!(answer.len() > OBJECT);
    assert!(reads > 2);
    rig.server.begin_drain();
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a drain does not wait past the idle time for a client that stalled in the middle
/// of a body: the request ends with its timeout, answered 400, and the drain ends with nothing
/// cut, though its deadline is thirty seconds, and nothing is stored.
#[tokio::test(start_paused = true)]
async fn a_drain_does_not_wait_past_the_idle_time_for_a_client_that_stalled() {
    let serving = Serving::start(idle_one_second).await;
    let rig = &serving.rig;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = serving.connect().unwrap();
    let stalled = Stalled::open(client, rig, &uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;
    rig.server.begin_drain();
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    let answer = stalled.answer().await;
    assert_eq!(status_of(&answer), 400);
    assert!(rig.hot_mem.get("k", None).await.is_err());
}

/// Asserts that an accept that fails does not stop the server: the connection that comes after it
/// is served, and the drain then ends as it does after any other.
#[tokio::test(start_paused = true)]
async fn a_failed_accept_is_followed_by_the_next_connection() {
    let serving = Serving::start_after(1, patient).await;
    let alive = probe(&serving, "/healthz").await;
    assert_eq!(status_of(&alive), 200);
    serving.rig.server.begin_drain();
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}
