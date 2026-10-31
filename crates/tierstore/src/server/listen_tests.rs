//! The sockets of a front that listens on more than one address, and the `listen` key that names
//! them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use super::{Addrs, bind, bind_all, has_duplicate, joined, local_addrs};
use crate::server::Drained;
use crate::server::testing::Rig;

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

/// Whether this host has IPv6 loopback, decided by trying to bind it.
fn has_v6() -> bool {
    std::net::TcpListener::bind(addr("[::1]:0")).is_ok()
}

/// The addresses a test listens on: IPv4 loopback and the IPv4 wildcard, which are two sockets
/// on every host, and IPv6 loopback where the host has it.
fn wanted() -> Vec<SocketAddr> {
    let mut wanted = vec![addr("127.0.0.1:0"), addr("0.0.0.0:0")];
    if has_v6() {
        wanted.push(addr("[::1]:0"));
    }
    wanted
}

/// The address a client dials to reach a socket bound at `at`: a wildcard is reached by way of
/// loopback.
fn reach(at: SocketAddr) -> SocketAddr {
    let ip = match at.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, at.port())
}

/// One request of `/healthz` at `at`, answered and closed: the whole reply.
async fn healthz(at: SocketAddr) -> String {
    let mut conn = TcpStream::connect(reach(at)).await.expect("connected");
    let request = "GET /healthz HTTP/1.1\r\nHost: tierstore\r\nConnection: close\r\n\r\n";
    conn.write_all(request.as_bytes()).await.expect("written");
    let mut reply = Vec::new();
    conn.read_to_end(&mut reply).await.expect("read");
    String::from_utf8_lossy(&reply).into_owned()
}

/// A server over `wanted`, serving all of them, with the addresses they are bound to and the
/// task that serves.
async fn started(wanted: &[SocketAddr]) -> (Rig, Vec<SocketAddr>, JoinHandle<Drained>) {
    let rig = Rig::with(1000, |config| config.drain_lb_wait = Duration::ZERO).await;
    let listeners = bind_all(wanted).expect("bound");
    let bound = local_addrs(&listeners).expect("addresses");
    let server = Arc::clone(&rig.server);
    let served = tokio::spawn(async move { server.serve_all(listeners).await });
    (rig, bound, served)
}

/// Asserts that one server takes the connections of every listener it is given: each address
/// answers `/healthz`, and the sockets are as many as were asked for, on ports of their own.
/// A server that served only the first would leave the others unanswered.
#[tokio::test]
async fn every_listener_serves() {
    let wanted = wanted();
    let (rig, bound, served) = started(&wanted).await;
    assert_eq!(bound.len(), wanted.len(), "{bound:?}");
    let mut ports: Vec<u16> = bound.iter().map(SocketAddr::port).collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports.len(), wanted.len(), "{bound:?}");
    for at in &bound {
        let reply = healthz(*at).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{at}: {reply}");
    }
    rig.server.begin_drain();
    assert_eq!(served.await.expect("served"), Drained { abandoned: 0 });
}

/// Asserts that the drain of the one server stops every listener: once it has ended, no address
/// takes a connection, the first or any other, and the server has none open.
#[tokio::test]
async fn the_drain_stops_every_listener() {
    let wanted = wanted();
    let (rig, bound, served) = started(&wanted).await;
    assert_eq!(bound.len(), wanted.len(), "{bound:?}");
    for at in &bound {
        let reply = healthz(*at).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{at}: {reply}");
    }
    rig.server.begin_drain();
    assert_eq!(served.await.expect("served"), Drained { abandoned: 0 });
    assert_eq!(rig.server.open_connections(), 0);
    for at in &bound {
        let refused = TcpStream::connect(reach(*at)).await;
        assert!(refused.is_err(), "{at} still takes connections");
    }
}

/// Asserts that the wildcard of each family can be bound on one port: `0.0.0.0:P` and `[::]:P`,
/// as a redirect of a remote packet needs, and that each takes the connections of its own family.
/// Without `IPV6_V6ONLY` the second bind is refused, as the IPv6 socket would hold the IPv4 port
/// too.
#[tokio::test]
async fn a_v6_wildcard_and_a_v4_wildcard_bind_on_one_port() {
    if !has_v6() {
        return;
    }
    let v4 = bind(addr("0.0.0.0:0")).expect("the v4 wildcard");
    let port = v4.local_addr().expect("an address").port();
    let v6 = bind(addr(&format!("[::]:{port}"))).expect("the v6 wildcard, on the same port");
    let (dialed, accepted) = tokio::join!(
        TcpStream::connect(reach(addr(&format!("0.0.0.0:{port}")))),
        v4.accept()
    );
    dialed.expect("dialed the v4 socket");
    assert!(accepted.expect("accepted").1.is_ipv4());
    let (dialed, accepted) = tokio::join!(
        TcpStream::connect(reach(addr(&format!("[::]:{port}")))),
        v6.accept()
    );
    dialed.expect("dialed the v6 socket");
    assert!(accepted.expect("accepted").1.is_ipv6());
}

/// Asserts that an address that cannot be bound fails the whole set and is the one the error
/// names, and that the reuse of an address (`SO_REUSEADDR`) does not let a second listener take
/// the address of a first.
#[tokio::test]
async fn an_address_that_cannot_be_bound_is_named_and_fails_the_set() {
    let held = bind(addr("127.0.0.1:0")).expect("a first listener");
    let busy = held.local_addr().expect("an address");
    let failed = bind_all(&[addr("0.0.0.0:0"), busy]).err();
    let (at, why) = failed.expect("the set is refused");
    assert_eq!(at, busy);
    assert_eq!(why.kind(), std::io::ErrorKind::AddrInUse);
}

/// Asserts what is the same address: the whole socket address, so the same IP on another port,
/// and another IP on the same port, are not.
#[test]
fn an_address_given_twice_is_a_duplicate() {
    let a = addr("192.0.2.1:9400");
    assert!(has_duplicate(&[a, addr("[::]:9400"), a]));
    assert!(!has_duplicate(&[]));
    assert!(!has_duplicate(&[a]));
    assert!(!has_duplicate(&[a, addr("192.0.2.1:9401")]));
    assert!(!has_duplicate(&[addr("0.0.0.0:9400"), addr("[::]:9400")]));
}

/// The addresses as the start line shows them.
#[test]
fn the_start_line_joins_the_addresses_with_commas() {
    let two = [addr("0.0.0.0:9400"), addr("[::]:9400")];
    assert_eq!(joined(&two), "0.0.0.0:9400,[::]:9400");
    assert_eq!(joined(&two[..1]), "0.0.0.0:9400");
}

/// A table of one `listen` key, as the tables of the file have it.
#[derive(Debug, Deserialize, Serialize)]
struct Table {
    listen: Addrs,
}

/// Asserts that a `listen` key reads one address as a string and several as a list, and writes
/// one address as a string, as it always was, and several as a list that reads back.
#[test]
fn a_listen_key_is_a_string_or_a_list_and_is_written_as_either() {
    let one: Table = toml::from_str("listen = \"192.0.2.1:9400\"").expect("a string");
    assert_eq!(one.listen.0, [addr("192.0.2.1:9400")]);
    let listed: Table = toml::from_str("listen = [\"192.0.2.1:9400\"]").expect("a list of one");
    assert_eq!(listed.listen, one.listen);
    let two: Table = toml::from_str("listen = [\"0.0.0.0:9400\", \"[::]:9400\"]").expect("a list");
    assert_eq!(two.listen.0, [addr("0.0.0.0:9400"), addr("[::]:9400")]);

    assert_eq!(
        toml::to_string(&one).expect("written"),
        "listen = \"192.0.2.1:9400\"\n"
    );
    let written = toml::to_string(&two).expect("written");
    let back: Table = toml::from_str(&written).expect("read back");
    assert_eq!(back.listen, two.listen);
    assert!(written.starts_with("listen = [\""), "{written}");
}

/// Asserts that a value that is no address is refused without showing it, in a list as alone.
#[test]
fn a_listen_value_that_is_no_address_is_refused_without_its_text() {
    for text in [
        "listen = \"secret-word\"",
        "listen = [\"192.0.2.1:9400\", \"secret-word\"]",
    ] {
        let why = toml::from_str::<Table>(text).expect_err("refused");
        assert!(!why.message().contains("secret-word"), "{}", why.message());
    }
}
