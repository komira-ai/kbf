//! A client that is slow or silent holds a connection for a bounded time only.
//!
//! The patience tests serve an in-memory connection on paused time: the clock moves on only
//! when every task is waiting, so the server's patience passes at once and the same way on
//! every run. The 5 s waits are on that clock too, and fail a test whose connection the
//! server never closed. The cap test serves real sockets on real time, and waits on the
//! listener's own permits, never on a clock.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_coordinator::http::Surface;
use kbf_coordinator::listen::{Listener, connection, serve_with_slots};
use kbf_types::HostRole;
use support::{Farm, client_config};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

const PATIENCE: Duration = Duration::from_millis(200);

/// The client end of an in-memory connection that the server end of is served on `surface`.
fn connected(farm: &Farm, surface: Surface) -> DuplexStream {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let tls = match surface {
        Surface::Heartbeat => farm.tls.0.clone(),
        Surface::Control => farm.tls.1.clone(),
    };
    tokio::spawn(connection(
        server,
        tls,
        surface,
        Arc::clone(&farm.coordinator),
        PATIENCE,
    ));
    client
}

async fn listener_of(farm: &Farm, surface: Surface, slots: Arc<Semaphore>) -> std::net::SocketAddr {
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bound");
    let addr = tcp.local_addr().expect("addr");
    let listener = Listener {
        tcp,
        tls: match surface {
            Surface::Heartbeat => farm.tls.0.clone(),
            Surface::Control => farm.tls.1.clone(),
        },
        surface,
    };
    assert_eq!(listener.local_addr().expect("addr"), addr);
    tokio::spawn(serve_with_slots(
        listener,
        Arc::clone(&farm.coordinator),
        PATIENCE,
        slots,
    ));
    addr
}

/// Catches: a client that connects and never starts its handshake holding the connection
/// open: the server closes it once the patience is out.
#[tokio::test(start_paused = true)]
async fn a_client_that_never_handshakes_is_dropped() {
    let farm = Farm::start("patience-handshake").await;
    let mut silent = connected(&farm, Surface::Heartbeat);
    let mut sink = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), silent.read_to_end(&mut sink));
    assert!(read.await.is_ok(), "the server closed the connection");
}

/// Catches: a client that finishes its handshake and then sends no request holding the
/// connection open: the server closes it once the patience is out.
#[tokio::test(start_paused = true)]
async fn a_client_that_sends_no_request_is_dropped() {
    let farm = Farm::start("patience-request").await;
    let who = farm.pki.updater("d1", HostRole::DAEMON);
    let config = client_config(&farm.pki, Some(&who));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let mut tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, connected(&farm, Surface::Heartbeat))
        .await
        .expect("a handshake");
    let mut sink = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut sink));
    assert!(read.await.is_ok(), "the server closed the connection");
}

/// Catches: a client that finishes the handshake, announces a body and sends none holding
/// the connection (and its buffers) open: the server answers 400 once the patience is out,
/// on the listener that asks for no client certificate, with no credentials.
#[tokio::test(start_paused = true)]
async fn a_client_that_stalls_its_request_body_is_answered_and_dropped() {
    let farm = Farm::start("patience-body").await;
    let config = client_config(&farm.pki, None);
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let mut tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, connected(&farm, Surface::Control))
        .await
        .expect("a handshake");
    let head = "POST /v1/desired HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4096\r\n\r\n";
    tls.write_all(head.as_bytes()).await.expect("sent");
    let mut sink = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut sink));
    let _ = read.await.expect("the server closed the connection");
    let text = String::from_utf8_lossy(&sink);
    assert!(text.starts_with("HTTP/1.1 400"), "{text}");
}

/// Catches: no cap on the connections of a listener: with a cap of one, a client that holds
/// the only slot keeps a second client waiting, and the second is served once the first is
/// gone. The slot is seen taken on the listener's own permits, and the second client's
/// handshake is waited for only after the first has gone.
#[tokio::test]
async fn a_listener_serves_no_more_connections_than_its_cap() {
    let farm = Farm::start("patience-cap").await;
    let slots = Arc::new(Semaphore::new(1));
    let addr = listener_of(&farm, Surface::Heartbeat, Arc::clone(&slots)).await;
    let first = TcpStream::connect(addr).await.expect("connected");
    let hang_guard = std::time::Instant::now();
    while slots.available_permits() != 0 {
        assert!(
            hang_guard.elapsed() < Duration::from_secs(60),
            "the first client never took the slot"
        );
        tokio::task::yield_now().await;
    }
    let who = farm.pki.updater("d1", HostRole::DAEMON);
    let config = client_config(&farm.pki, Some(&who));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let tcp = TcpStream::connect(addr)
        .await
        .expect("queued in the backlog");
    let second = tokio::spawn(tokio_rustls::TlsConnector::from(config).connect(name, tcp));
    assert_eq!(slots.available_permits(), 0, "the slot is held");
    drop(first);
    let late = tokio::time::timeout(Duration::from_secs(60), second).await;
    assert!(
        late.expect("served after the slot freed")
            .expect("joined")
            .is_ok()
    );
}
