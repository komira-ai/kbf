//! A client that is slow or silent holds a connection for a bounded time only.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_coordinator::http::Surface;
use kbf_coordinator::listen::{Listener, serve};
use kbf_types::HostRole;
use support::{Farm, client_config};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

async fn patient_listener(farm: &Farm) -> std::net::SocketAddr {
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bound");
    let addr = tcp.local_addr().expect("addr");
    let listener = Listener {
        tcp,
        tls: farm.tls.0.clone(),
        surface: Surface::Heartbeat,
    };
    assert_eq!(listener.local_addr().expect("addr"), addr);
    tokio::spawn(serve(
        listener,
        Arc::clone(&farm.coordinator),
        Duration::from_millis(200),
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
