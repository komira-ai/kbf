//! The drain of a server whose listening socket is the service manager's
//! ([`ServerConfig::activated`]), on the paused clock over in-memory pipes. No test measures time.

use crate::Backend;

use super::pipes::{Serving, Stalled, call, drained, probe, read_answer, status_of, until};
use super::testing::{BUCKET, Rig};
use super::{Drained, REFUSING, ServerConfig};

/// The body of the put these tests hold in flight, of which the first four bytes are sent.
const BODY: &[u8] = b"0123456789";

/// A drain that waits a moment for nothing (there is no load balancer to tell) and as long as
/// the requests need, over bodies that may idle for an hour, on an activated server.
fn activated(config: &mut ServerConfig) {
    config.activated = true;
    config.drain_lb_wait = std::time::Duration::ZERO;
    config.drain_deadline = std::time::Duration::from_secs(3600);
    config.body_idle_timeout = std::time::Duration::from_secs(3600);
}

/// Asserts the drain of an activated server while a put holds it open: once the drain begins every
/// answer says `Connection: close`; once the server stops accepting, a connection that is idle
/// is closed at once, without a request and without waiting for the put (the plain drain keeps it
/// open and answers its next request 503, see `drain_tests.rs`); and the put is answered as if
/// there were no drain and the drain then ends with nothing cut.
#[tokio::test(start_paused = true)]
async fn an_activated_drain_closes_an_idle_connection_while_a_request_is_in_flight() {
    let serving = Serving::start(activated).await;
    let rig = &serving.rig;
    until(&rig.server, |server| server.open_connections() == 0).await;
    let mut idle = serving.connect().unwrap();
    until(&rig.server, |server| server.open_connections() == 1).await;
    let uri = format!("/{BUCKET}/k");
    let create = [("if-none-match", "*")];
    let client = serving.connect().unwrap();
    let held = Stalled::open(client, rig, &uri, &create, BODY, 4).await;
    until(&rig.server, |server| server.in_flight() == 1).await;

    rig.server.begin_drain();
    serving.refusing().await;
    // Closed with no request sent: the answer is the end of the connection.
    assert_eq!(read_answer(&mut idle).await, "");
    assert!(!serving.served.is_finished());
    assert_eq!(rig.server.in_flight(), 1);

    let answer = held.finish().await;
    assert_eq!(status_of(&answer), 200);
    assert_eq!(drained(serving.served).await, Drained { abandoned: 0 });
    assert_eq!(rig.hot_mem.get("k", None).await.unwrap(), "0123456789");
}

/// Asserts that the answers of an activated server say `Connection: close` from the moment the
/// drain begins and not before: an answer before it has no such header, `/readyz` and a signed
/// get after it both have it.
#[tokio::test(start_paused = true)]
async fn an_activated_server_closes_after_every_answer_once_the_drain_begins() {
    let serving = Serving::start(|config| {
        activated(config);
        config.drain_lb_wait = std::time::Duration::from_secs(3600);
    })
    .await;
    let rig = &serving.rig;
    let before = rig.send("GET", &format!("/{BUCKET}/k"), &[], b"").await;
    assert_eq!(before.header("connection"), "");

    rig.server.begin_drain();
    let ready = probe(&serving, "/readyz").await;
    assert_eq!(status_of(&ready), 503);
    assert!(ready.contains("connection: close"), "{ready}");
    let got = call(&serving, "GET", &format!("/{BUCKET}/k"), &[], b"").await;
    assert_eq!(status_of(&got), 404);
    assert!(got.contains("connection: close"), "{got}");
}

/// Asserts that an activated server serves a request that arrives after it stopped accepting, on
/// a connection it had, as it would before, with `Connection: close`: not 503 `ServiceUnavailable`,
/// which is what the plain drain answers, and which a request that was on the wire when the server
/// told its connections to close would otherwise get. The same request to a server that is not
/// activated is the 503.
#[tokio::test(start_paused = true)]
async fn an_activated_server_does_not_answer_503_after_it_stopped_accepting() {
    let activated = Rig::with(1 << 20, |config| config.activated = true).await;
    let plain = Rig::with(1 << 20, |_| {}).await;
    for rig in [&activated, &plain] {
        rig.server
            .phase
            .store(REFUSING, std::sync::atomic::Ordering::SeqCst);
    }
    let served = activated.get("k", None).await;
    assert_eq!(served.status, 404);
    assert_eq!(served.header("connection"), "close");
    let refused = plain.get("k", None).await;
    assert_eq!(refused.status, 503);
}
