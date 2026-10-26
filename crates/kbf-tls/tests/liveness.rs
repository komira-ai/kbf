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
/// Kills: a TLS connection whose end of stream is not seen, so a lost connection is read
/// as a client's abort. Mutant: mark gone in the `Plain` arm of `poll_read` only: this
/// goes red.
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
/// Kills: a read error taken for nothing, so that a reset connection stays live. Mutant:
/// mark gone on a zero-length read only: this goes red.
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
/// Kills: a lost connection counted as a client's abort because h2's write failed before
/// any read did. Mutant: drop the `mark_gone` after the write in `each_io!`: this goes
/// red.
#[tokio::test]
async fn a_connection_is_gone_when_a_write_fails() {
    assert!(gone_after_a_failed_write(false).await);
}

/// Asserts the same for the vectored write h2 uses.
///
/// Kills: a vectored write whose failure is not seen. Mutant: mark gone only in
/// `poll_write`: this goes red.
#[tokio::test]
async fn a_connection_is_gone_when_a_vectored_write_fails() {
    assert!(gone_after_a_failed_write(true).await);
}

/// Asserts that a connection the server drops is gone though nothing was read or failed:
/// the server-initiated close (a keepalive timeout) is told by the drop.
///
/// Kills: a connection dropped by the server that stays live. Mutant: remove the `Drop`
/// impl of `Conn`: this goes red.
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
