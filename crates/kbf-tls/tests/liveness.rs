//! When an [`Incoming`] connection counts as gone: a peer's close on either kind of
//! connection, a socket that fails on a read or on a write, and a connection the server
//! drops. Every test reads the connection's own [`Liveness`], the flag a stream's end is
//! told apart by.

mod support;

use std::io::IoSlice;

use futures::StreamExt as _;
use kbf_tls::{ConnInfo, Incoming};
use support::{Files, ca, connect, dir, echo, server_leaf, tcp, until};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tonic::transport::server::Connected as _;

/// Asserts that a TLS connection is gone once its peer closes it (sends close_notify and
/// shuts the socket).
///
/// Catches: a TLS connection whose end of stream is not seen, so a lost connection is read
/// as a client's abort (only the `Plain` arm of `poll_read` marking the connection gone).
#[tokio::test]
async fn a_tls_connection_is_gone_when_its_peer_closes_it() {
    let ca = ca("liveness CA");
    let files = Files::write(&dir("liveness-tls"), &server_leaf(&ca), None);
    let mut server = echo(tcp(), Some(files.source()));
    let mut peer = connect(server.addr, &ca.pem, None).await.expect("connect");
    let info = server.infos.recv().await.expect("the connection");
    support::ping(&mut peer).await.expect("ping");
    assert!(!info.live.is_gone(), "a connection that is being used");

    peer.shutdown().await.expect("close_notify");
    until("the close is seen", || info.live.is_gone()).await;
}

/// Asserts that a connection whose socket the peer resets is gone: the server's read
/// fails.
///
/// Catches: a read error taken for nothing, so that a reset connection stays live (only a
/// zero-length read marking the connection gone).
#[tokio::test]
async fn a_connection_is_gone_when_its_socket_is_reset() {
    let mut server = echo(tcp(), None);
    let mut peer = TcpStream::connect(server.addr).await.expect("connect");
    let info = server.infos.recv().await.expect("the connection");
    peer.write_all(b"ping").await.expect("write");
    let mut back = [0u8; 4];
    peer.read_exact(&mut back).await.expect("echo");
    assert!(!info.live.is_gone(), "a connection that is being used");

    peer.set_zero_linger().expect("linger");
    drop(peer);
    until("the reset is seen", || info.live.is_gone()).await;
}

/// A server that accepts one plain connection and writes to it, never reading, until a
/// write fails; the connection stays open after. Gives its address, the connection's
/// [`ConnInfo`] and a receiver that resolves when the write has failed.
fn writer(
    vectored: bool,
) -> (
    std::net::SocketAddr,
    oneshot::Receiver<ConnInfo>,
    oneshot::Receiver<()>,
) {
    let tcp = tcp();
    let addr = tcp.local_addr().expect("local address");
    let mut incoming = Incoming::new(tcp, None);
    let (failed, failure) = oneshot::channel();
    let (send, info) = oneshot::channel();
    tokio::spawn(async move {
        let mut conn = incoming
            .next()
            .await
            .expect("a connection")
            .expect("accept");
        let _ = send.send(conn.connect_info());
        let block = [0u8; 1024];
        loop {
            let wrote = if vectored {
                conn.write_vectored(&[IoSlice::new(&block)]).await
            } else {
                conn.write(&block).await
            };
            if wrote.is_err() {
                break;
            }
        }
        let _ = failed.send(());
        // Held open: only the failed write can have marked it gone.
        std::future::pending::<()>().await;
    });
    (addr, info, failure)
}

/// A connection whose server never reads, reset by its peer while the server writes with
/// `vectored` or plain writes: whether it is gone once a write has failed.
async fn gone_after_a_failed_write(vectored: bool) -> bool {
    let (addr, info, failure) = writer(vectored);
    let peer = TcpStream::connect(addr).await.expect("connect");
    let info = info.await.expect("the connection");
    peer.set_zero_linger().expect("linger");
    drop(peer);
    failure.await.expect("a write failed");
    info.live.is_gone()
}

/// Asserts that a connection is gone once a write to it fails, with no read having seen
/// the peer go and the connection still open: the server pushes to a peer that has reset.
///
/// Catches: a lost connection counted as a client's abort because h2's write failed before
/// any read did (the `mark_gone` after the write in `each_io!` dropped).
#[tokio::test]
async fn a_connection_is_gone_when_a_write_fails() {
    assert!(gone_after_a_failed_write(false).await);
}

/// Asserts the same for the vectored write h2 uses.
///
/// Catches: a vectored write whose failure is not seen (only `poll_write` marking the
/// connection gone).
#[tokio::test]
async fn a_connection_is_gone_when_a_vectored_write_fails() {
    assert!(gone_after_a_failed_write(true).await);
}

/// Asserts that a connection the server drops is gone though nothing was read or failed:
/// the server-initiated close (a keepalive timeout) is told by the drop.
///
/// Catches: a connection dropped by the server that stays live (the `Drop` impl of `Conn`
/// removed).
#[tokio::test]
async fn a_connection_the_server_drops_is_gone() {
    let tcp = tcp();
    let addr = tcp.local_addr().expect("local address");
    let mut incoming = Incoming::new(tcp, None);
    let _peer = TcpStream::connect(addr).await.expect("connect");
    let conn = incoming
        .next()
        .await
        .expect("a connection")
        .expect("accept");
    let info = conn.connect_info();
    assert!(!info.live.is_gone(), "a connection nobody has closed");
    drop(conn);
    assert!(info.live.is_gone());
}

/// Asserts that a read into a buffer with no room is not the end of the stream: the
/// connection stays live, a read with room still returns what the peer sent, and only the
/// peer's close then marks it gone.
///
/// Catches: a read that completes Ok with nothing filled because the buffer was full
/// taken for the stream's end, so that a live connection is counted as gone (and an
/// Execute that ends later as dropped instead of a client abort).
#[tokio::test]
async fn a_read_with_no_room_does_not_end_the_connection() {
    let tcp = tcp();
    let addr = tcp.local_addr().expect("local address");
    let mut incoming = Incoming::new(tcp, None);
    let mut peer = TcpStream::connect(addr).await.expect("connect");
    let mut conn = incoming
        .next()
        .await
        .expect("a connection")
        .expect("accept");
    let info = conn.connect_info();
    // Data pending, so the read with no room completes at once.
    peer.write_all(b"x").await.expect("write");
    let empty = conn.read(&mut []).await.expect("a read with no room");
    assert_eq!(empty, 0);
    assert!(!info.live.is_gone(), "a read with no room says nothing");

    let mut byte = [0u8; 1];
    assert_eq!(conn.read(&mut byte).await.expect("read"), 1);
    assert!(!info.live.is_gone(), "a read that returned data");

    peer.shutdown().await.expect("close");
    assert_eq!(conn.read(&mut byte).await.expect("read"), 0);
    assert!(info.live.is_gone(), "the peer's close is the end");
}
