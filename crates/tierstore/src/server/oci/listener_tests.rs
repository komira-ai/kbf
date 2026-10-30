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
use super::{
    OciError, OciFront, OciListener, OciMetrics, OciMode, OciPrefix, acceptor, plain_allowed,
};
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
    let looping = tokio::spawn(accept_loop(incoming, Some(tls), front, limits, stopped));
    tokio::time::sleep(ACCEPT_PAUSE * 2).await;
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    stop.send(true).expect("a loop");
    assert_eq!(looping.await.expect("the loop ended"), 0);
}

fn plain_front() -> (Arc<OciFront>, OciLimits) {
    let prefix = OciPrefix::new("oci/").expect("a prefix");
    let metrics = Arc::new(OciMetrics::default());
    let backend = Arc::new(MemoryBackend::new());
    let front = Arc::new(OciFront::new(backend, prefix, metrics));
    let second = Duration::from_secs(1);
    let limits = OciLimits {
        max_connections: 1,
        handshake_timeout: second,
        header_timeout: second,
        write_idle: second,
        lifetime: second,
        drain_deadline: second,
    };
    (front, limits)
}

/// Asserts that plain HTTP is allowed on the loopback addresses (anywhere in the IPv4 loopback
/// block, and `::1`) and on no other: not on the unspecified addresses, a documentation address,
/// or an IPv4-mapped address of a host that is not this one.
#[test]
fn plain_http_is_allowed_on_loopback_only() {
    let block = SocketAddr::from(([127, 255, 0, 9], 9500));
    for allowed in ["127.0.0.1:0", "[::1]:9500"] {
        let addr: SocketAddr = allowed.parse().expect("an address");
        assert!(plain_allowed(addr).is_ok(), "{allowed}");
    }
    assert!(plain_allowed(block).is_ok(), "{block}");
    for refused in [
        "0.0.0.0:9500",
        "[::]:9500",
        "192.0.2.1:9500",
        "198.51.100.7:9500",
        "[2001:db8::1]:9500",
        "[::ffff:192.0.2.1]:9500",
    ] {
        let addr: SocketAddr = refused.parse().expect("an address");
        let error = plain_allowed(addr).expect_err(refused);
        assert!(matches!(error, OciError::PlainNotLoopback { .. }));
        assert!(error.is_invalid_flags());
        let text = error.to_string();
        assert!(text.contains(refused), "{text}");
        assert!(
            text.contains("plain HTTP is served only on loopback"),
            "{text}"
        );
        assert!(
            text.contains("needs --oci-tls-cert and --oci-tls-key"),
            "{text}"
        );
    }
}

/// Asserts that the listener refuses a plain bind off loopback before it binds anything, serves
/// plain on loopback, and says its mode.
#[tokio::test]
async fn a_plain_listener_binds_on_loopback_only() {
    let (front, limits) = plain_front();
    let off: SocketAddr = "0.0.0.0:0".parse().expect("an address");
    let refused = OciListener::bind_plain(off, Arc::clone(&front), limits).await;
    let error = refused.err().expect("refused");
    assert!(matches!(error, OciError::PlainNotLoopback { addr } if addr == off));

    let on: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let plain = OciListener::bind_plain(on, front, limits)
        .await
        .expect("bound");
    assert_eq!(plain.mode(), OciMode::Http);
    assert!(plain.local_addr().expect("an address").ip().is_loopback());

    let dir = TestDir::under(std::env::temp_dir(), "oci-listener-mode");
    let files = leaf(dir.path(), "front", &ca(), vec![ip("127.0.0.1")]);
    let tls = acceptor(&files.cert, &files.key, on).expect("an acceptor");
    let (front, limits) = plain_front();
    let secure = OciListener::bind(on, tls, front, limits)
        .await
        .expect("bound");
    assert_eq!(secure.mode(), OciMode::Https);
    assert_eq!(OciMode::Http.as_str(), "http");
    assert_eq!(OciMode::Https.as_str(), "https");
}
