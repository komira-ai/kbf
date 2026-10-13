//! Every listener (REAPI, worker, operator API) turns Nagle's algorithm off
//! (`TCP_NODELAY`) on each connection it accepts. With it on, a small write that
//! follows another unacknowledged small write waits for the peer's delayed ACK
//! (about 40 ms on Linux), so every sequential small call, a `FindMissingBlobs` or a
//! small `Read`, cost about 40 ms.
//!
//! The server runs in this process, so the test finds the socket the server accepted
//! for its own connection among this process's descriptors and reads the option off it.

mod support;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

use support::{Cell, PROMPT};

#[tokio::test]
async fn every_listener_turns_nagle_off_on_the_connections_it_accepts() {
    let cell = Cell::start_with_api().await;
    let listeners = [
        ("REAPI", cell.reapi_addr),
        ("worker", cell.worker_addr),
        ("operator API", cell.api.expect("an operator API")),
    ];
    let mut nagle_on = Vec::new();
    for (name, listener) in listeners {
        let client = TcpStream::connect(listener).expect("connect");
        let peer = client.local_addr().expect("the client's address");
        if !nodelay(accepted_socket(listener, peer).await) {
            nagle_on.push(name);
        }
    }
    assert!(
        nagle_on.is_empty(),
        "Nagle's algorithm left on: {nagle_on:?}"
    );
}

/// The descriptor of the socket the server accepted on `listener` for the connection
/// from `peer`, once the server has accepted it.
async fn accepted_socket(listener: SocketAddr, peer: SocketAddr) -> libc::c_int {
    let deadline = Instant::now() + PROMPT;
    loop {
        let found = (0..descriptor_limit()).find(|&fd| {
            address(fd, libc::getsockname) == Some(listener)
                && address(fd, libc::getpeername) == Some(peer)
        });
        if let Some(fd) = found {
            return fd;
        }
        assert!(
            Instant::now() < deadline,
            "{listener} accepted no connection from {peer}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// One past the highest descriptor number this process may hold.
fn descriptor_limit() -> libc::c_int {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) writes one rlimit into `limit`, which is one.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
        0
    );
    libc::c_int::try_from(limit.rlim_cur.min(1 << 16)).expect("fits")
}

type NameFn =
    unsafe extern "C" fn(libc::c_int, *mut libc::sockaddr, *mut libc::socklen_t) -> libc::c_int;

/// The IPv4 address `name` (getsockname or getpeername) reports for `fd`; `None` for a
/// number that is no socket, or is not IPv4. Only a read: `fd` may belong to any thread
/// of this process, and one that is closed or reused meanwhile answers an error or that
/// socket's address.
fn address(fd: libc::c_int, name: NameFn) -> Option<SocketAddr> {
    // SAFETY: sockaddr_in is plain integers, so all zeros is a valid value.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let size = libc::socklen_t::try_from(size_of::<libc::sockaddr_in>()).expect("fits");
    let mut len = size;
    // SAFETY: `sin` holds `len` bytes; the call writes at most that many and stores
    // the address's full length in `len`.
    let answered = unsafe { name(fd, (&raw mut sin).cast(), &raw mut len) };
    let ipv4 = answered == 0 && len == size && libc::c_int::from(sin.sin_family) == libc::AF_INET;
    ipv4.then(|| {
        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
        SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(sin.sin_port)))
    })
}

/// Whether `TCP_NODELAY` is set on the socket `fd`.
fn nodelay(fd: libc::c_int) -> bool {
    let mut on: libc::c_int = 0;
    let mut len = libc::socklen_t::try_from(size_of::<libc::c_int>()).expect("fits");
    // SAFETY: `on` holds `len` bytes; the call writes at most that many.
    let answered = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            (&raw mut on).cast(),
            &raw mut len,
        )
    };
    assert_eq!(answered, 0, "getsockopt(TCP_NODELAY) on {fd}");
    on != 0
}
