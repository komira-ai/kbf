//! The drain, run on the paused clock over in-memory pipes: the accept loop takes its connections
//! from a channel, and each test counts what happens (requests in flight, connections open, the
//! requests the deadline cut) and awaits it. No test here measures time, and none waits on a
//! real clock.

use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncWriteExt, DuplexStream};

use crate::Backend;

use super::pipes::{
    OBJECT, Serving, Stalled, begin_get, call, drained, exchange, head, probe, put_big,
    read_response, read_rest, read_rest_slowly, send, status_of, until,
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

/// A connection that has had one answer, which kept it open, and now waits for its next request.
async fn kept_idle(serving: &Serving) -> DuplexStream {
    let rig = &serving.rig;
    let _ = rig.hot_mem.put("warm", Bytes::from_static(b"w")).await;
    let mut client = serving.connect().unwrap();
    let uri = format!("/{BUCKET}/warm");
    let answer = exchange(&mut client, rig, "GET", &uri, &[], b"").await;
    assert_eq!(status_of(&answer), 200);
    assert!(!answer.contains("connection: close"), "{answer}");
    client
}

/// Reads `client` to its end and asserts that the server sent nothing before it: the server has
/// shut its end of the connection.
async fn assert_shut(client: &mut DuplexStream) {
    let mut rest = Vec::new();
    read_rest(client, &mut rest).await;
    assert!(rest.is_empty(), "{}", String::from_utf8_lossy(&rest));
}

/// Asserts the sequence of a drain: `/readyz` is 503 at once, the S3 calls are still served
/// through the settle, then, while a request in flight holds the drain open, a connection that
/// sits idle is not answered 503 but has the server's end shut (the client reads the end of the
/// stream) and a new connection is refused, and the drain ends with nothing cut once that request
/// is done and the idle connection is closed.
#[tokio::test(start_paused = true)]
async fn the_drain_serves_through_the_settle_then_stops_accepting_and_ends() {
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
    assert_shut(&mut idle).await;
    assert!(serving.connect().is_none());
    assert!(!serving.served.is_finished());

    let finished = held.finish().await;
    assert_eq!(status_of(&finished), 200);
    drop(idle);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that from the moment the drain begins every answer carries `Connection: close`: a get
/// on a connection that the client kept open, though the answer to its earlier request did not say
/// close, and the probe `/readyz`, whose request did not ask for the connection to close either.
/// A client that reads the header does not send its next request on a connection the server is
/// about to close. It would catch a drain that sets the header only once it has stopped accepting.
#[tokio::test(start_paused = true)]
async fn every_answer_after_the_drain_begins_carries_connection_close() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let mut client = kept_idle(&serving).await;
    let mut other = serving.connect().unwrap();
    rig.server.begin_drain();

    let uri = format!("/{BUCKET}/warm");
    let during = exchange(&mut client, rig, "GET", &uri, &[], b"").await;
    assert_eq!(status_of(&during), 200);
    assert!(during.contains("connection: close"), "{during}");
    let text = "GET /readyz HTTP/1.1\r\nhost: tier.test\r\n\r\n";
    other.write_all(text.as_bytes()).await.unwrap();
    let ready = read_response(&mut other).await;
    assert_eq!(status_of(&ready), 503);
    assert!(ready.contains("connection: close"), "{ready}");

    drop((client, other));
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a request whose head began to arrive before the settle ended, on a connection that
/// had been idle, is served once the rest of it arrives after the settle: 200 with
/// `Connection: close`, not 503 `ServiceUnavailable` and not a closed connection. It would catch a
/// server that refuses what it is in the middle of reading, and one that shuts a connection
/// without asking whether a request is on its way.
#[tokio::test(start_paused = true)]
async fn a_request_on_the_wire_when_the_settle_ends_is_served() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let mut client = kept_idle(&serving).await;
    let uri = format!("/{BUCKET}/warm");
    let text = head(rig, "GET", &uri, &[], b"", "");
    let (first, second) = text.split_at(text.len() / 2);
    client.write_all(first.as_bytes()).await.unwrap();
    rig.server.begin_drain();
    serving.refusing().await;

    client.write_all(second.as_bytes()).await.unwrap();
    let answer = read_response(&mut client).await;
    assert_eq!(status_of(&answer), 200, "{answer}");
    assert!(answer.contains("connection: close"), "{answer}");
    drop(client);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a connection that is idle when the settle ends sees the server's write half shut,
/// the client reads the end of the stream and nothing before it, and that the server then holds
/// the connection, reading, until the client closes its end: the drain does not end first, and it
/// does when the client has closed. A plain close would reset a connection whose client sent
/// something meanwhile.
#[tokio::test(start_paused = true)]
async fn an_idle_connection_at_the_end_of_the_settle_is_shut_and_held_until_the_client_closes() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let mut client = kept_idle(&serving).await;
    rig.server.begin_drain();
    assert_shut(&mut client).await;
    assert_eq!(rig.server.lingering_connections(), 1);
    assert!(!serving.served.is_finished());

    client.shutdown().await.unwrap();
    until(&rig.server, |server| server.lingering_connections() == 0).await;
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a lingering close is finite: a client that never closes its end does not hold
/// the drain open for ever; the server closes the connection when the bound has passed, the drain
/// ends with nothing cut, and the client's next write finds the connection gone. The bound is
/// `header_timeout`, which the test leaves as it is; the test waits for the end of the drain and
/// asserts nothing about how long it takes.
#[tokio::test(start_paused = true)]
async fn a_lingering_close_ends_when_the_client_never_closes() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let mut client = kept_idle(&serving).await;
    rig.server.begin_drain();
    assert_shut(&mut client).await;
    assert_eq!(rig.server.lingering_connections(), 1);

    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    assert_eq!(rig.server.lingering_connections(), 0);
    assert!(client.write_all(b"x").await.is_err());
}

/// Asserts that a request written after the server's end was shut is read, not served, and
/// counted: the object it would create does not exist, the late counter is 1 and `/metricz` says
/// so. It would catch a drain that serves what it can no longer answer cleanly, one that closes
/// without reading (the client would be reset), and a count that does not happen.
#[tokio::test(start_paused = true)]
async fn a_request_written_after_the_servers_fin_is_read_dropped_and_counted() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let mut client = kept_idle(&serving).await;
    rig.server.begin_drain();
    assert_shut(&mut client).await;
    assert_eq!(rig.server.late_requests(), 0);

    let create = [("if-none-match", "*")];
    let uri = format!("/{BUCKET}/late");
    send(&mut client, rig, "PUT", &uri, &create, b"late").await;
    until(&rig.server, |server| server.late_requests() == 1).await;
    assert!(rig.hot_mem.get("late", None).await.is_err());
    let text = rig.server.metrics_probe();
    assert!(
        text.contains("tierstore_drain_late_requests_total 1\n"),
        "{text}"
    );
    drop(client);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that a request in flight when the settle ends, on a connection the client kept alive,
/// finishes and is answered 200 with `Connection: close`, beside an idle connection that is shut
/// and held, and that the drain is not over until both are done.
#[tokio::test(start_paused = true)]
async fn a_request_in_flight_finishes_beside_a_lingering_connection() {
    let serving = Serving::start(patient).await;
    let rig = &serving.rig;
    let mut idle = kept_idle(&serving).await;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = serving.connect().unwrap();
    let held = Stalled::open_kept(client, rig, &uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;
    rig.server.begin_drain();
    serving.refusing().await;

    assert_shut(&mut idle).await;
    assert_eq!(rig.server.lingering_connections(), 1);
    assert_eq!(rig.server.in_flight(), 1);
    assert!(!serving.served.is_finished());
    let answer = held.finish().await;
    assert_eq!(status_of(&answer), 200);
    assert!(answer.contains("connection: close"), "{answer}");
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), "0123456789");
    drop(idle);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
}

/// Asserts that the deadline still cuts a request in flight when an idle connection is held in its
/// lingering close: the drain ends at the deadline with one request cut (the held connection is
/// not a request and is not counted), the held connection is still held, and the request goes on.
#[tokio::test(start_paused = true)]
async fn the_deadline_cuts_a_request_in_flight_beside_a_lingering_connection() {
    let serving = Serving::start(|config| {
        config.drain_lb_wait = Duration::from_millis(20);
        config.drain_deadline = Duration::from_millis(300);
        config.body_idle_timeout = Duration::from_secs(3600);
    })
    .await;
    let rig = &serving.rig;
    let idle = kept_idle(&serving).await;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = serving.connect().unwrap();
    let held = Stalled::open_kept(client, rig, &uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;
    rig.server.begin_drain();

    assert_eq!(drained(serving.served).await, Drained { abandoned: 1 });
    assert_eq!(rig.server.lingering_connections(), 1);
    assert_eq!(rig.server.in_flight(), 1);
    let answer = held.finish().await;
    assert_eq!(status_of(&answer), 200);
    drop(idle);
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
