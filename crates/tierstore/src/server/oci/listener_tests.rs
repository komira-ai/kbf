//! The accept loop of the OCI listener over a source of connections that fails, on the paused
//! clock: no socket.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::listener::{OciLimits, accept_loop};
use super::test_pki::{ca, ip, leaf};
use super::{OciFront, OciMetrics, OciPrefix, acceptor};
use crate::MemoryBackend;
use crate::server::{ACCEPT_PAUSE, Incoming};

/// A source whose first accept fails and whose others never complete; it counts the asks.
struct Failing {
    asked: Arc<AtomicUsize>,
}

impl Incoming for Failing {
    type Stream = TcpStream;

    fn poll_incoming(
        &mut self,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        match self.asked.fetch_add(1, Ordering::SeqCst) {
            0 => Poll::Ready(Err(io::Error::other("an accept that failed"))),
            _ => Poll::Pending,
        }
    }
}

/// Asserts that a failed accept does not end the loop or take a connection: after the pause the
/// loop asks again, and a stop then ends it with nothing cut.
#[tokio::test(start_paused = true)]
async fn a_failed_accept_is_followed_by_another_ask() {
    let dir = TestDir::under(std::env::temp_dir(), "oci-listener-accept");
    let files = leaf(dir.path(), "front", &ca(), vec![ip("127.0.0.1")]);
    let listen: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let tls = acceptor(&files.cert, &files.key, listen).expect("an acceptor");
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let metrics = Arc::new(OciMetrics::default());
    let front = Arc::new(OciFront::new(
        Arc::new(MemoryBackend::new()),
        prefix,
        metrics,
    ));
    let second = Duration::from_secs(1);
    let limits = OciLimits {
        max_connections: 1,
        handshake_timeout: second,
        header_timeout: second,
        write_idle: second,
        lifetime: second,
        drain_deadline: second,
    };
    let asked = Arc::new(AtomicUsize::new(0));
    let incoming = Failing {
        asked: Arc::clone(&asked),
    };
    let (stop, stopped) = watch::channel(false);
    let looping = tokio::spawn(accept_loop(incoming, tls, front, limits, stopped));
    tokio::time::sleep(ACCEPT_PAUSE * 2).await;
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    stop.send(true).expect("a loop");
    assert_eq!(looping.await.expect("the loop ended"), 0);
}
