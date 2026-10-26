//! The drain over a real listener: how it is wired to the socket that the server accepts on. The
//! sequence of the drain, and its deadline, are tested in the crate, on a paused clock over
//! in-memory pipes (`src/server/drain_tests.rs`).

use std::time::Duration;

use reqwest::Client;
use tierstore::server::Drained;
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
