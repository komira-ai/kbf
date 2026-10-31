//! The drain over a real listener: how it is wired to the socket that the server accepts on. The
//! sequence of the drain, and its deadline, are tested in the crate, on a paused clock over
//! in-memory pipes (`src/server/drain_tests.rs`).

use std::time::Duration;

use reqwest::Client;
use tierstore::server::Drained;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::support::Harness;

/// How long the server goes on serving once the drain began. The test waits for the drain to
/// end and asserts nothing about how long it takes.
const LB_WAIT: Duration = Duration::from_millis(20);

/// The longest the test waits for the drain to end. It only stops a test that hangs, and then
/// the test fails.
const SAFETY: Duration = Duration::from_secs(60);

/// Asserts that a drain over a real listener ends with nothing cut when the only connection is a
/// kept-alive one that has had its answer and has nothing in flight, that the server has no
/// connection open once it has ended, and that it takes no new connection then: dropping the
/// listener is what refuses them.
#[tokio::test]
async fn a_drain_closes_the_idle_connections_and_the_listener_and_ends() {
    let started = Harness::start_fixed(|config| config.drain_lb_wait = LB_WAIT);
    let h = started.await;
    let client = Client::new();
    let health = format!("http://{}/healthz", h.addr);
    let probe = client.get(&health).send().await.unwrap();
    assert_eq!(probe.status(), 200);
    assert_eq!(probe.text().await.unwrap(), "ok");
    h.server.begin_drain();
    let drained = tokio::time::timeout(SAFETY, h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
    assert_eq!(h.server.open_connections(), 0);
    assert!(TcpStream::connect(h.addr).await.is_err());
}

/// What a client sends to ask `/healthz`, keeping the connection alive.
const HEALTHZ: &[u8] = b"GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// Asserts, over a real socket, that a connection that is idle when the settle ends gets a clean
/// end of stream, not a reset, that the server then holds it open while the drain waits, so that
/// a request the client writes after the end of stream is read and not answered with a reset, and
/// that the drain ends once the client has closed. It would catch a drain that closes an idle
/// connection with `close()` (a reset for a client that wrote meanwhile), and one that ends
/// without waiting for the client.
#[tokio::test]
async fn an_idle_connection_gets_a_clean_end_of_stream_and_is_held_until_it_closes() {
    let started = Harness::start_fixed(|config| config.drain_lb_wait = LB_WAIT);
    let h = started.await;
    let mut client = TcpStream::connect(h.addr).await.unwrap();
    client.write_all(HEALTHZ).await.unwrap();
    let mut answer = Vec::new();
    let mut piece = [0_u8; 1024];
    while !answer.ends_with(b"\r\n\r\nok") {
        let read = client.read(&mut piece).await.unwrap();
        assert!(read > 0, "the connection ended inside the answer");
        answer.extend_from_slice(&piece[..read]);
    }
    h.server.begin_drain();

    let first = tokio::time::timeout(SAFETY, client.read(&mut piece)).await;
    assert_eq!(first.unwrap().unwrap(), 0, "a clean end of stream");
    assert_eq!(h.server.lingering_connections(), 1);
    client.write_all(HEALTHZ).await.unwrap();
    let second = tokio::time::timeout(SAFETY, client.read(&mut piece)).await;
    assert_eq!(second.unwrap().unwrap(), 0, "still the end of stream");
    assert!(!h.served.is_finished());

    client.shutdown().await.unwrap();
    let drained = tokio::time::timeout(SAFETY, h.served).await;
    assert_eq!(drained.unwrap().unwrap(), Drained { abandoned: 0 });
}
