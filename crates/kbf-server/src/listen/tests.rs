//! Sockets bound, adopted and stopped.

use std::net::TcpStream;
use std::os::fd::OwnedFd;

use futures::{FutureExt as _, StreamExt as _};

use super::*;

fn any() -> SocketAddr {
    "127.0.0.1:0".parse().expect("address")
}

/// Catches: a spare handle where none was asked for (a drain would leave the socket
/// accepting into a backlog nobody serves), none where one was (a handover has nothing to
/// offer), a spare that is another socket (the successor would serve elsewhere), and an
/// address already in use bound anyway or reported without the address.
#[test]
fn a_socket_has_a_spare_handle_on_itself_only_when_asked() {
    let plain = Socket::bind(any(), false).expect("bind");
    assert!(plain.spare.is_none());
    let with = Socket::bind(any(), true).expect("bind");
    let spare = with.spare.as_ref().expect("a spare handle");
    let same = TcpListener::from(spare.try_clone().expect("a copy"));
    assert_eq!(same.local_addr().expect("address"), with.local);

    let taken = Socket::bind(with.local, false).err().expect("in use");
    assert!(
        taken.to_string().contains(&with.local.to_string()),
        "{taken}"
    );
}

use std::net::TcpListener;

/// Catches: a socket adopted that listens elsewhere than the flags say (a successor would
/// serve an address the checks on its flags never saw), one that is not a socket at all,
/// and an adopted socket that cannot hand itself on again (the successor of the successor
/// would find nothing to take).
#[test]
fn an_adopted_socket_must_listen_where_the_flags_say() {
    let listener = TcpListener::bind(any()).expect("bind");
    let local = listener.local_addr().expect("address");
    let adopted = Socket::adopt(
        OwnedFd::from(listener.try_clone().expect("copy")),
        local,
        true,
    )
    .expect("a socket on the right address");
    assert_eq!(adopted.local, local);
    assert!(adopted.spare.is_some());
    let plain = Socket::adopt(
        OwnedFd::from(listener.try_clone().expect("copy")),
        local,
        false,
    )
    .expect("a socket on the right address");
    assert!(plain.spare.is_none());

    let wrong: SocketAddr = "127.0.0.1:9".parse().expect("address");
    let error = Socket::adopt(OwnedFd::from(listener), wrong, false)
        .err()
        .expect("wrong address");
    let said = error.to_string();
    assert!(
        said.contains("bind 127.0.0.1:9") && said.contains(&local.to_string()),
        "{said}"
    );

    let (not_a_socket, _) = std::os::unix::net::UnixStream::pair().expect("a pair");
    let error = Socket::adopt(OwnedFd::from(not_a_socket), wrong, false)
        .err()
        .expect("not TCP");
    assert!(error.to_string().starts_with("bind 127.0.0.1:9"), "{error}");
}

/// Catches: a gRPC listener that does not serve connections, one that serves them with
/// Nagle's algorithm on (small replies would wait for a delayed ACK), and the latch: it
/// must end the stream of connections and close the socket at once, wherever the server
/// is in accepting (here: asleep waiting for a connection that never comes), so that
/// nothing accepts on the address afterwards; closing again changes nothing, and neither
/// does closing a listener nobody waits on.
#[tokio::test]
async fn a_grpc_listener_accepts_until_latched_and_then_closes_its_socket() {
    let socket = Socket::bind(any(), false).expect("bind");
    let addr = socket.local;
    let (mut incoming, latch) = socket.grpc(None).expect("registered");
    assert!(
        incoming.next().now_or_never().is_none(),
        "a connection from nowhere"
    );

    let client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let accepted = incoming
        .next()
        .await
        .expect("a connection")
        .expect("accepted");
    assert!(accepted.remote_addr().is_some());
    drop((client, accepted));

    // A task asleep in accept sees the end when the latch closes, though nothing else wakes it.
    let waiting = tokio::spawn(async move { incoming.next().await.is_none() });
    tokio::task::yield_now().await;
    latch.close();
    assert!(
        waiting.await.expect("the task"),
        "still accepting after the latch"
    );
    assert!(
        TcpStream::connect(addr).is_err(),
        "the address still accepts"
    );
    latch.close();

    let socket = Socket::bind(any(), false).expect("bind");
    let (incoming, latch) = socket.grpc(None).expect("registered");
    latch.close();
    drop(incoming);
}

/// Catches: a listener that keeps its socket when the server drops it while a latch (held
/// by a signal that outlives the server) still exists: the address must be free again.
#[tokio::test]
async fn dropping_a_grpc_listener_closes_its_socket_whoever_holds_the_latch() {
    let socket = Socket::bind(any(), false).expect("bind");
    let addr = socket.local;
    let (incoming, _latch) = socket.grpc(None).expect("registered");
    assert!(TcpStream::connect(addr).is_ok());
    drop(incoming);
    assert!(
        TcpStream::connect(addr).is_err(),
        "the address still accepts"
    );
}

/// Catches: an operator API listener that is not the socket bound: it must answer on the
/// address the socket has.
#[tokio::test]
async fn an_http_listener_is_the_socket_bound() {
    let socket = Socket::bind(any(), false).expect("bind");
    let addr = socket.local;
    let listener = socket.http().expect("registered");
    assert_eq!(listener.local_addr().expect("address"), addr);
    assert!(TcpStream::connect(addr).is_ok());
}

/// Catches: a socket error that does not say which address it was for.
#[test]
fn a_socket_error_names_its_address() {
    let addr: SocketAddr = "127.0.0.1:9".parse().expect("address");
    let error = bind_error(addr)(io::Error::other("no reactor"));
    assert_eq!(error.to_string(), "bind 127.0.0.1:9: no reactor");
}

fn unavailable() -> io::Error {
    io::Error::from(io::ErrorKind::AddrNotAvailable)
}

fn listener() -> TcpListener {
    TcpListener::bind(any()).expect("bind")
}

/// Catches: no retry of an address that is not there yet (the frontend stays down after a
/// reboot), and waits that do not start at 250 ms and double.
#[test]
fn an_unavailable_address_is_retried_until_it_binds() {
    let mut script = vec![Err(unavailable()), Err(unavailable()), Ok(listener())].into_iter();
    let mut waits = Vec::new();
    let mut attempts = 0;
    let got = retry_unavailable(Duration::from_secs(90), &mut |d| waits.push(d), &mut || {
        attempts += 1;
        script.next().expect("no more attempts")
    });
    assert!(got.is_ok());
    assert_eq!(attempts, 3);
    assert_eq!(
        waits,
        [Duration::from_millis(250), Duration::from_millis(500)]
    );
}

/// Catches: a retry of every error (a port in use or a refused bind would wait out the
/// budget before it is reported).
#[test]
fn any_other_bind_error_is_at_once_fatal() {
    for kind in [io::ErrorKind::AddrInUse, io::ErrorKind::PermissionDenied] {
        let mut attempts = 0;
        let mut slept = 0;
        let got = retry_unavailable(Duration::from_secs(90), &mut |_| slept += 1, &mut || {
            attempts += 1;
            Err(io::Error::from(kind))
        });
        assert_eq!(got.err().map(|e| e.kind()), Some(kind));
        assert_eq!((attempts, slept), (1, 0));
    }
}

/// Catches: no bound on the retry (a start that never ends, killed by systemd), and a
/// bound that is exceeded by more than the one wait that crosses it.
#[test]
fn the_retry_ends_when_the_waits_reach_the_budget() {
    let mut waits = Vec::new();
    let mut attempts = 0;
    let budget = Duration::from_secs(10);
    let got = retry_unavailable(budget, &mut |d| waits.push(d), &mut || {
        attempts += 1;
        assert!(attempts < 100, "unbounded");
        Err(unavailable())
    });
    assert_eq!(
        got.err().map(|e| e.kind()),
        Some(io::ErrorKind::AddrNotAvailable)
    );
    let total: Duration = waits.iter().sum();
    assert!(total >= budget, "{total:?}");
    assert!(total - *waits.last().expect("a wait") < budget);
    assert_eq!(attempts, waits.len() + 1);
    assert!(waits.iter().all(|w| *w <= Duration::from_secs(2)));
}

/// Catches the production bind path losing the address's own error once it gives up:
/// binding a documentation address (192.0.2.1, on no interface) with a small budget runs
/// the real wait between attempts and then fails with `AddrNotAvailable`, which the
/// frontend reports as it is. (The retry rule itself is pinned by the `retry_unavailable`
/// tests above.) Mutant: `bind_within` returns its failure as `ErrorKind::Other`.
#[test]
fn a_real_bind_to_an_absent_address_waits_then_says_it_is_not_available() {
    let absent: SocketAddr = "192.0.2.1:0".parse().expect("an address");
    let error = bind_within(absent, Duration::from_millis(300)).expect_err("not on this host");
    assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable, "{error}");
}
