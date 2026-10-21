//! A TCP forwarder in front of a server, for tests that kill and revive a node.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinHandle};

/// A TCP forwarder in front of a node: `cut` closes every connection through it and
/// drops new ones until `restore`, which is how a node dies and comes back.
pub struct Switch {
    pub addr: SocketAddr,
    up: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<AbortHandle>>>,
    _accept: JoinHandle<()>,
}

impl Switch {
    pub async fn start(target: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let up = Arc::new(AtomicBool::new(true));
        let connections: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
        let accept = {
            let (up, connections) = (Arc::clone(&up), Arc::clone(&connections));
            tokio::spawn(async move {
                loop {
                    // A loopback listener's accept fails only when the process is out of
                    // descriptors. The panic ends only this accept task: a later connect is then
                    // refused or hangs, so the test that hit it fails there.
                    let (mut inbound, _) = listener.accept().await.expect("accept");
                    if !up.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Ok(mut outbound) = TcpStream::connect(target).await else {
                        continue;
                    };
                    let copy = tokio::spawn(async move {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                    connections.lock().expect("lock").push(copy.abort_handle());
                }
            })
        };
        Self {
            addr,
            up,
            connections,
            _accept: accept,
        }
    }

    /// The node is gone: its connections close, new ones are dropped.
    pub fn cut(&self) {
        self.up.store(false, Ordering::SeqCst);
        for connection in self.connections.lock().expect("lock").drain(..) {
            connection.abort();
        }
    }

    /// The node is back.
    pub fn restore(&self) {
        self.up.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// An echo server on loopback: every connection's bytes come straight back.
    async fn echo() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept");
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.split();
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                });
            }
        });
        addr
    }

    async fn round_trip(stream: &mut TcpStream) -> std::io::Result<[u8; 4]> {
        stream.write_all(b"ping").await?;
        let mut back = [0; 4];
        stream.read_exact(&mut back).await?;
        Ok(back)
    }

    /// A connection through the switch reaches the server; `cut` ends the open
    /// connection and refuses new ones; `restore` lets a new connection through. A
    /// switch that left a connection open, or never let one through again, fails here.
    #[tokio::test]
    async fn cut_ends_connections_and_restore_lets_new_ones_through() {
        let switch = Switch::start(echo().await).await;
        let mut open = TcpStream::connect(switch.addr).await.expect("connect");
        assert_eq!(&round_trip(&mut open).await.expect("through"), b"ping");

        switch.cut();
        // The abort closes the forwarder's halves of the connection: a read ends.
        let mut rest = Vec::new();
        let _ = open.read_to_end(&mut rest).await;
        assert!(rest.is_empty(), "{rest:?}");

        // A new connection is accepted and dropped at once: nothing comes back.
        let mut refused = TcpStream::connect(switch.addr).await.expect("connect");
        assert!(round_trip(&mut refused).await.is_err());

        switch.restore();
        let mut again = TcpStream::connect(switch.addr).await.expect("connect");
        assert_eq!(&round_trip(&mut again).await.expect("through"), b"ping");
    }

    /// A server that is not there: the switch accepts, cannot reach it and drops the
    /// connection, so nothing comes back. Catches a forwarder that hangs the caller when its
    /// target is down (for example one that keeps the inbound stream open). A forwarder
    /// that panics is not caught: the panic unwinds the task and drops the inbound stream,
    /// so the client still sees the connection close.
    #[tokio::test]
    async fn a_dead_target_drops_the_connection() {
        let dead = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("address")
        };
        let switch = Switch::start(dead).await;
        let mut stream = TcpStream::connect(switch.addr).await.expect("connect");
        assert!(round_trip(&mut stream).await.is_err());
    }
}
