//! The activated descriptor: what is taken and what is refused.

use std::net::TcpListener;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener, UnixStream};

use crate::listener::{Listen, adopt, open};
use crate::testutil::scratch;

/// A duplicate of `fd`'s object that the test owns (and closes when it is dropped), so that
/// `adopt` can be given a descriptor without taking the original.
fn dup(fd: &impl AsRawFd) -> OwnedFd {
    // SAFETY: `fd` is open for the call; the new descriptor is owned here.
    let raw = unsafe { libc::dup(fd.as_raw_fd()) };
    assert!(raw >= 0, "dup");
    // SAFETY: `raw` is a fresh descriptor nothing else owns.
    unsafe { std::os::fd::FromRawFd::from_raw_fd(raw) }
}

/// Catches an activated descriptor taken without looking at what it is: only a listening unix
/// stream socket is adopted, and the adopted listener is the same socket (a client of the
/// path is accepted on it). A datagram socket, a TCP listener, a connected (not listening)
/// stream and a descriptor that is not open are each refused with a reason, and a refused
/// descriptor is not closed by the refusal.
#[test]
fn only_a_listening_unix_stream_socket_is_adopted() {
    let dir = scratch("adopt");
    let path = dir.join("l.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let ours = dup(&listener);
    let adopted = adopt(ours.into_raw_fd()).expect("a listening unix stream socket");
    let _client = UnixStream::connect(&path).expect("connect");
    adopted.accept().expect("the adopted listener accepts");

    let datagram = UnixDatagram::bind(dir.join("d.sock")).expect("datagram");
    let tcp = TcpListener::bind("127.0.0.1:0").expect("tcp");
    let (connected, _peer) = UnixStream::pair().expect("pair");
    for (fd, why) in [
        (dup(&datagram), "not a stream socket"),
        (dup(&tcp), "not a unix socket"),
        (dup(&connected), "not a listening socket"),
    ] {
        let e = adopt(fd.as_raw_fd()).expect_err(why);
        assert!(e.contains(why), "{e}");
        // Left open: the refusal did not close it.
        assert!(adopt(fd.as_raw_fd()).is_err());
        // SAFETY: fcntl F_GETFD on an open descriptor has no other precondition.
        assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } >= 0);
    }
    // A number no process has open (above any descriptor limit; a number just closed could
    // be reused by another test's thread).
    let e = adopt(1_000_000).expect_err("closed");
    assert!(e.contains("not a socket"), "{e}");
}

/// Catches an activated socket without a path being accepted (the stop request has nowhere to
/// connect), and the path being taken from the wrong place for one that has one.
#[test]
fn an_activated_socket_needs_a_path_and_is_not_owned() {
    let abstract_addr = SocketAddr::from_abstract_name(b"kbf-updater-test-no-path").expect("addr");
    let nameless = UnixListener::bind_addr(&abstract_addr).expect("abstract bind");
    let e = open(&Listen::Activated(dup(&nameless).into_raw_fd()))
        .err()
        .expect("no path");
    assert!(e.contains("no path"), "{e}");

    let dir = scratch("opened");
    let path = dir.join("l.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let opened = open(&Listen::Activated(dup(&listener).into_raw_fd())).expect("open");
    assert_eq!(
        (opened.path.as_path(), opened.owned),
        (path.as_path(), false)
    );
    let own = open(&Listen::Bind(dir.join("b.sock"))).expect("bind");
    assert!(own.owned);
    assert_eq!(own.path, dir.join("b.sock"));
    let e = open(&Listen::Bind(dir.join("missing/b.sock")))
        .err()
        .expect("bind error");
    assert!(e.starts_with("socket "), "{e}");
}
