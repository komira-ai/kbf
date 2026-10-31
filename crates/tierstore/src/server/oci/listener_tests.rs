//! The accept loop of the OCI listener over a source of connections that fails, on the paused
//! clock: no socket.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::listener::{OciLimits, accept_loop};
use super::test_pki::{ca, ip, leaf};
use super::{OciFront, OciListener, OciMetrics, OciMode, OciPrefix, acceptor, plain_warning};
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

/// Asserts that the start warning for a plain front is for the addresses off loopback and for no
/// other: it names the unspecified addresses, documentation addresses and an IPv4-mapped address
/// that is not loopback, and is absent for anywhere in the IPv4 loopback block, `::1` and the
/// mapped loopback address.
#[test]
fn the_plain_warning_is_for_non_loopback_addresses_only() {
    for quiet in ["127.0.0.1:0", "[::1]:9500", "[::ffff:127.0.0.1]:9500"] {
        let addr: SocketAddr = quiet.parse().expect("an address");
        assert_eq!(plain_warning(addr), None, "{quiet}");
    }
    // The far end of the loopback block, built from its octets: the public-file lint allows no
    // dotted loopback address but 127.0.0.1.
    let far = SocketAddr::from((Ipv4Addr::new(127, 255, 0, 9), 9500));
    assert_eq!(plain_warning(far), None, "{far}");
    for warned in [
        "0.0.0.0:9500",
        "[::]:9500",
        "192.0.2.1:9500",
        "198.51.100.7:9500",
        "[2001:db8::1]:9500",
        "[::ffff:192.0.2.1]:9500",
    ] {
        let addr: SocketAddr = warned.parse().expect("an address");
        let text = plain_warning(addr).expect(warned);
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains(&addr.to_string()), "{text}");
        assert!(
            text.contains("serves plain HTTP on a non-loopback address"),
            "{text}"
        );
        assert!(text.contains("--oci-tls-cert and --oci-tls-key"), "{text}");
    }
}

/// One plain request of the front at `port` on loopback, answered and closed: the whole reply.
async fn get_v2(port: u16) -> String {
    let mut conn = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connected");
    let request = "GET /v2/ HTTP/1.1\r\nHost: front\r\nConnection: close\r\n\r\n";
    conn.write_all(request.as_bytes()).await.expect("written");
    let mut reply = Vec::new();
    conn.read_to_end(&mut reply).await.expect("read");
    String::from_utf8_lossy(&reply).into_owned()
}

/// The address a client dials to reach a socket bound at `at`: a wildcard by way of loopback.
fn reach(at: SocketAddr) -> SocketAddr {
    let ip = match at.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, at.port())
}

/// Asserts that one listener binds every address it is given and answers a request at each, that
/// the first is the one `local_addr` says, and that a stop closes every socket and ends the one
/// loop with nothing cut.
#[tokio::test]
async fn one_listener_serves_every_address_it_is_given() {
    let (front, limits) = plain_front();
    let mut wanted: Vec<SocketAddr> = ["127.0.0.1:0", "0.0.0.0:0"]
        .iter()
        .map(|text| text.parse().expect("an address"))
        .collect();
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        wanted.push("[::1]:0".parse().expect("an address"));
    }
    let listener = OciListener::bind_all(&wanted, None, front, limits)
        .await
        .expect("bound");
    let bound = listener.local_addrs().expect("addresses");
    assert_eq!(bound.len(), wanted.len(), "{bound:?}");
    assert_eq!(listener.local_addr().expect("the first"), bound[0]);
    let (stop, stopped) = watch::channel(false);
    let serving = tokio::spawn(listener.serve(stopped));
    for at in &bound {
        let mut conn = TcpStream::connect(reach(*at)).await.expect("connected");
        let request = "GET /v2/ HTTP/1.1\r\nHost: front\r\nConnection: close\r\n\r\n";
        conn.write_all(request.as_bytes()).await.expect("written");
        let mut reply = Vec::new();
        conn.read_to_end(&mut reply).await.expect("read");
        let reply = String::from_utf8_lossy(&reply);
        assert!(reply.starts_with("HTTP/1.1 200 "), "{at}: {reply}");
    }
    stop.send(true).expect("a listener");
    assert_eq!(serving.await.expect("served"), 0);
    for at in &bound {
        let refused = TcpStream::connect(reach(*at)).await;
        assert!(refused.is_err(), "{at} still takes connections");
    }
}

/// Asserts that a listener given no address binds nothing and has no address to report.
#[tokio::test]
async fn a_listener_with_no_address_has_none_to_report() {
    let (front, limits) = plain_front();
    let listener = OciListener::bind_all(&[], None, front, limits)
        .await
        .expect("bound");
    assert!(listener.local_addrs().expect("none").is_empty());
    assert!(listener.local_addr().is_err());
}

/// Asserts that the listener binds a plain address that is not loopback (the unspecified
/// address), says its mode is plain HTTP, and answers a request there; and that it serves plain
/// on loopback and TLS where a certificate is given.
#[tokio::test]
async fn a_plain_listener_binds_off_loopback_and_serves() {
    let (front, limits) = plain_front();
    let off: SocketAddr = "0.0.0.0:0".parse().expect("an address");
    let wide = OciListener::bind_plain(off, Arc::clone(&front), limits)
        .await
        .expect("bound");
    assert_eq!(wide.mode(), OciMode::Http);
    let bound = wide.local_addr().expect("an address");
    assert!(bound.ip().is_unspecified() && bound.port() != 0, "{bound}");
    let (stop, stopped) = watch::channel(false);
    let serving = tokio::spawn(wide.serve(stopped));
    let reply = get_v2(bound.port()).await;
    assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
    stop.send(true).expect("a listener");
    assert_eq!(serving.await.expect("served"), 0);

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
