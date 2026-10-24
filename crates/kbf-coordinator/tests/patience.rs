//! A client that is slow or silent holds a connection for a bounded time only.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_coordinator::http::Surface;
use kbf_coordinator::listen::{Listener, serve};
use kbf_types::HostRole;
use support::{Farm, client_config};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn patient_listener(farm: &Farm) -> std::net::SocketAddr {
    listener_of(farm, Surface::Heartbeat, 1024).await
}

async fn listener_of(farm: &Farm, surface: Surface, connections: usize) -> std::net::SocketAddr {
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
    tokio::spawn(serve(
        listener,
        Arc::clone(&farm.coordinator),
        Duration::from_millis(200),
        connections,
    ));
    addr
}

/// Catches: a client that connects and never starts its handshake holding the connection
/// open: the server closes it once the patience is out.
#[tokio::test]
async fn a_client_that_never_handshakes_is_dropped() {
    let farm = Farm::start("patience-handshake").await;
    let addr = patient_listener(&farm).await;
    let mut silent = TcpStream::connect(addr).await.expect("connected");
    let mut sink = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), silent.read_to_end(&mut sink));
    assert!(read.await.is_ok(), "the server closed the connection");
}

/// Catches: a client that finishes its handshake and then sends no request holding the
/// connection open: the server closes it once the patience is out.
#[tokio::test]
async fn a_client_that_sends_no_request_is_dropped() {
    let farm = Farm::start("patience-request").await;
    let addr = patient_listener(&farm).await;
    let who = farm.pki.updater("d1", HostRole::Daemon);
    let config = client_config(&farm.pki, Some(&who));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let tcp = TcpStream::connect(addr).await.expect("connected");
    let mut tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
        .await
        .expect("a handshake");
    let mut sink = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut sink));
    assert!(read.await.is_ok(), "the server closed the connection");
}

/// Catches: a client that finishes the handshake, announces a body and sends none holding
/// the connection (and its buffers) open: the server answers 400 once the patience is out,
/// on the listener that asks for no client certificate, with no credentials.
#[tokio::test]
async fn a_client_that_stalls_its_request_body_is_answered_and_dropped() {
    let farm = Farm::start("patience-body").await;
    let addr = listener_of(&farm, Surface::Control, 1024).await;
    let config = client_config(&farm.pki, None);
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let tcp = TcpStream::connect(addr).await.expect("connected");
    let mut tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
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
/// the only slot (silent, until the patience drops it) keeps a second client waiting, and
/// the second is served once the first is gone.
#[tokio::test]
async fn a_listener_serves_no_more_connections_than_its_cap() {
    let farm = Farm::start("patience-cap").await;
    let addr = listener_of(&farm, Surface::Heartbeat, 1).await;
    let mut first = TcpStream::connect(addr).await.expect("connected");
    let who = farm.pki.updater("d1", HostRole::Daemon);
    let config = client_config(&farm.pki, Some(&who));
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let tcp = TcpStream::connect(addr)
        .await
        .expect("queued in the backlog");
    let second = tokio_rustls::TlsConnector::from(config).connect(name, tcp);
    let mut second = Box::pin(second);
    let early = tokio::time::timeout(Duration::from_millis(100), &mut second).await;
    assert!(
        early.is_err(),
        "the second client waits while the slot is held"
    );
    let mut sink = Vec::new();
    let _ = first.read_to_end(&mut sink).await;
    let late = tokio::time::timeout(Duration::from_secs(5), second).await;
    assert!(late.expect("served after the slot freed").is_ok());
}
