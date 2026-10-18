//! Loopback ports for tests: ports nothing listens on, and ports a server is to bind
//! later, without the window between "probed free" and "bound" that a socket bound to
//! `127.0.0.1:0` and dropped leaves open.
//!
//! In that window another test binding `127.0.0.1:0`, or a connection's source port,
//! takes the number. A test that wants a dead port then reaches the thief (a TLS
//! server answering where "connection refused" was expected), and a server that is
//! to bind the number later fails with "Address already in use".
//!
//! - [`dead_addr`]: an address on which connecting is refused, for as long as the test
//!   binary runs.
//! - [`Reserved`]: a port held until the test is ready, and then turned into the
//!   listener (no second bind, so nothing can come between).
//! - [`own_loopback_port`]: for a port a *child process* or code that binds by itself
//!   will bind, so no socket can be handed over.
//!
//! # What holds a port and refuses
//!
//! - Linux: a TCP socket that is bound and not listening owns its port (a second
//!   `bind` fails with `EADDRINUSE`; neither this module nor `std` sets
//!   `SO_REUSEADDR` on the held one) and a connection attempt to it is answered with a
//!   reset: `connect` fails with "connection refused".
//! - macOS: that socket owns its port too, but a SYN that finds it is dropped without
//!   a reset (the socket is in the lookup tables in state CLOSED, and the stack drops
//!   segments for a CLOSED socket), so `connect` waits out its whole timeout and
//!   fails with "timed out". So the held socket is also connected, to a peer socket of
//!   this process (a listener on a port of its own, whose accepted connection is
//!   kept): a connected socket is found only under its full address pair, so a SYN
//!   from anyone else finds no socket, is answered with a reset, and the port stays
//!   owned: the kernel skips a port that is in use when it picks one (a bind of
//!   port 0, a connection's source port), and a bind without `SO_REUSEADDR` fails
//!   with `EADDRINUSE`. A bind of exactly that number *with* `SO_REUSEADDR` (what
//!   `std` does) succeeds on macOS, so a test that probes by hand uses [`is_taken`].
//!   (Connecting the socket to its own address, which would need no peer, is
//!   refused by macOS with `EINVAL`.) [`Reserved::listen`] has to drop that socket
//!   and its peer and bind the listener; between the two a thief would need to draw
//!   exactly that number from the kernel's random ephemeral range within
//!   microseconds (on Linux the socket itself listens and there is no gap).
//!
//! The tests below prove the behaviour on the platform they run on, macOS included.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use rustix::net::{self, AddressFamily, SocketType};

/// A loopback port held by a socket that owns it and refuses connections to it (bound
/// and not listening on Linux; also connected to a peer on macOS, see the module docs).
///
/// The IPv4 loopback port is held; the same port number on `::1` is held too when the
/// host has IPv6 loopback and the number is free there (best effort: a test that
/// dials `::1` with this number then gets "refused" as well).
#[derive(Debug)]
pub struct Reserved {
    v4: Held,
    // Held for its lifetime only.
    _v6: Option<Held>,
    port: u16,
}

impl Reserved {
    /// Holds a fresh port on `127.0.0.1` (and the same number on `::1` if possible).
    ///
    /// # Panics
    /// If the socket cannot be made or bound: a test cannot go on without a port.
    #[must_use]
    pub fn new() -> Self {
        let v4 = new_v4();
        let port = local_port(&v4.fd);
        let v6 = held_socket(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).ok();
        Self { v4, _v6: v6, port }
    }

    /// The port number.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The held address on `127.0.0.1`.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }

    /// Starts listening on the held socket itself: the port was never free between
    /// "refused" and "accepting".
    ///
    /// # Panics
    /// If `listen` fails.
    #[must_use]
    pub fn listen(self) -> TcpListener {
        #[cfg(target_os = "linux")]
        let fd = self.v4.fd;
        #[cfg(not(target_os = "linux"))]
        let fd = {
            // A connected socket cannot listen: release it and its peer and bind the listener
            // (SO_REUSEADDR for the closing connection's leftover state).
            let addr = self.addr();
            drop(self.v4);
            let fd = net::socket(AddressFamily::INET, SocketType::STREAM, None).expect("socket");
            net::sockopt::set_socket_reuseaddr(&fd, true).expect("reuseaddr");
            net::bind(&fd, &addr).expect("bind the held port");
            fd
        };
        net::listen(&fd, 128).expect("listen");
        TcpListener::from(fd)
    }
}

/// The IPv4 half of a new [`Reserved`]: a free number on `127.0.0.1`, held.
fn new_v4() -> Held {
    held_socket(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).expect("bind a loopback socket")
}

impl Default for Reserved {
    fn default() -> Self {
        Self::new()
    }
}

/// A socket bound to `addr`, never listening and without `SO_REUSEADDR`.
fn bound_socket(addr: SocketAddr) -> rustix::io::Result<OwnedFd> {
    let family = if addr.is_ipv4() {
        AddressFamily::INET
    } else {
        AddressFamily::INET6
    };
    let fd = net::socket(family, SocketType::STREAM, None)?;
    net::bind(&fd, &addr)?;
    Ok(fd)
}

/// A held port: the socket that owns it and, on macOS, the accepted end of the
/// connection it was made with, kept open so that connection stays established.
#[derive(Debug)]
struct Held {
    fd: OwnedFd,
    _peer: Option<OwnedFd>,
}

/// A socket that owns `addr` and refuses connections to it on every platform: bound
/// and connected to a peer socket of this process (see the module docs for why macOS
/// needs that). The held port of a [`Reserved`] on macOS uses it; Linux tests it too.
#[cfg(any(not(target_os = "linux"), test))]
fn connected_socket(addr: SocketAddr) -> rustix::io::Result<Held> {
    let fd = bound_socket(addr)?;
    // A listener on a port of its own, used for this one connection and then closed.
    let listener = bound_socket(SocketAddr::new(addr.ip(), 0))?;
    net::listen(&listener, 1)?;
    net::connect(&fd, &SocketAddr::new(addr.ip(), local_port(&listener)))?;
    let peer = net::accept(&listener)?;
    Ok(Held {
        fd,
        _peer: Some(peer),
    })
}

/// The socket that holds `addr`: bound only, on Linux.
#[cfg(target_os = "linux")]
fn held_socket(addr: SocketAddr) -> rustix::io::Result<Held> {
    Ok(Held {
        fd: bound_socket(addr)?,
        _peer: None,
    })
}

/// The socket that holds `addr`: connected to a peer, elsewhere.
#[cfg(not(target_os = "linux"))]
fn held_socket(addr: SocketAddr) -> rustix::io::Result<Held> {
    connected_socket(addr)
}

fn local_port(fd: &OwnedFd) -> u16 {
    let addr = net::getsockname(fd).expect("getsockname");
    SocketAddr::try_from(addr).expect("an inet address").port()
}

/// Whether another socket cannot bind `addr` because its port is owned: the check the
/// tests of held ports make instead of binding by hand.
///
/// On Linux the other socket sets `SO_REUSEADDR`, as `std` does (the strictest thief).
/// On macOS a socket with `SO_REUSEADDR` can bind a port whose owner is a connected
/// socket (the owner of a [`Reserved`] there), so the check is a bind without it,
/// which fails with `EADDRINUSE`; what keeps the other binders away on macOS is that
/// the kernel does not give an owned port to a bind of port 0 or to a connection's
/// source port.
#[must_use]
pub fn is_taken(addr: SocketAddr) -> bool {
    cannot_bind(addr)
}

#[cfg(target_os = "linux")]
fn cannot_bind(addr: SocketAddr) -> bool {
    TcpListener::bind(addr).is_err()
}

#[cfg(not(target_os = "linux"))]
fn cannot_bind(addr: SocketAddr) -> bool {
    matches!(bound_socket(addr), Err(rustix::io::Errno::ADDRINUSE))
}

/// An address on `127.0.0.1` on which connecting is refused until the test binary
/// exits: its port is held by a socket kept in a process-wide list. See the module
/// docs for why that port is dead and cannot be taken.
///
/// # Panics
/// If no socket can be bound.
#[must_use]
pub fn dead_addr() -> SocketAddr {
    static HELD: Mutex<Vec<Reserved>> = Mutex::new(Vec::new());
    let held = Reserved::new();
    let addr = held.addr();
    HELD.lock().expect("held ports").push(held);
    addr
}

/// An address of this call's own, with a port nothing listens on yet, for a port that
/// code outside the test (a child process, a function that binds for itself) will bind
/// and the test cannot hold.
///
/// A port number probed on `127.0.0.1` is free only until a test binding
/// `127.0.0.1:0`, or a connection's source port, takes it. So on Linux the address is
/// a loopback address of this call's own (127 and three more bytes: the
/// whole loopback block is the host's own there) that nothing else binds, with a port
/// probed on it; the one thing that could take the port is a process that chose the
/// same address. Elsewhere (macOS has only `127.0.0.1`) this is a probed port on
/// `127.0.0.1` and the window stays; there, prefer [`Reserved::listen`].
///
/// # What the address guarantees (Linux)
/// The address is per process and per call: it is derived from the process id and a
/// counter of this process's calls (`pid * 16 + call`, kept to 23 bits). Two calls in
/// one process never share an address. Two concurrent test processes can share one
/// only if their pids are congruent modulo 2^19 (524288) and they are at the same call
/// number; with the counter under 15 no other pair of inputs gives the same address.
/// On other platforms every call returns `127.0.0.1` with a probed port, so nothing
/// above holds there.
///
/// # Panics
/// After 15 calls in one process, or if no socket can be bound.
#[must_use]
pub fn own_loopback_port() -> SocketAddr {
    let ip = own_loopback_ip();
    let probe = TcpListener::bind(SocketAddr::new(ip, 0)).expect("bind");
    probe.local_addr().expect("addr")
}

#[cfg(target_os = "linux")]
fn own_loopback_ip() -> IpAddr {
    use std::sync::atomic::{AtomicU32, Ordering};
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    assert!(call < 15, "more addresses than one process can tell apart");
    // 23 bits of process and call number under a leading 1 bit: the second byte is
    // never 0, so never the usual loopback address, and with a call number under 15
    // the three bytes are never all ones (the block's broadcast address).
    let host = ((std::process::id().wrapping_mul(16) + call) & 0x7F_FFFF) | 0x80_0000;
    let [_, a, b, c] = host.to_be_bytes();
    IpAddr::from([127, a, b, c])
}

#[cfg(not(target_os = "linux"))]
fn own_loopback_ip() -> IpAddr {
    IpAddr::from(Ipv4Addr::LOCALHOST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    /// Catches a held port that can be taken (the old probe-and-drop helper: the
    /// thief binds `127.0.0.1:port` and the "dead" port answers), and a held port
    /// that accepts connections (a listening socket would). Both on the same socket.
    #[test]
    fn a_held_port_cannot_be_taken_and_refuses_connections() {
        let held = Reserved::new();
        assert!(is_taken(held.addr()), "a thief took the held port");
        let err = TcpStream::connect(held.addr()).expect_err("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "{err}");
    }

    /// Catches a `listen` that rebinds (a window for a thief) or loses the port: the
    /// listener answers on the very address that was refused, and a thief that tried
    /// in between failed.
    #[test]
    fn a_held_port_becomes_the_listener_on_the_same_address() {
        let held = Reserved::new();
        let addr = held.addr();
        assert!(is_taken(addr), "a thief took the port");
        assert!(TcpStream::connect(addr).is_err());
        let listener = held.listen();
        assert_eq!(listener.local_addr().expect("addr"), addr);
        TcpStream::connect(addr).expect("accepted");
    }

    /// Catches a `Default` that does not hold a port (the same as `new`): the default
    /// value owns a port nobody else can bind and that refuses connections.
    #[test]
    fn the_default_value_holds_a_port() {
        let held = Reserved::default();
        assert_ne!(held.port(), 0);
        assert!(is_taken(held.addr()), "a thief took it");
        let err = TcpStream::connect(held.addr()).expect_err("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "{err}");
    }

    /// Catches `dead_addr` handing out a number it did not keep: it stays taken after
    /// the call returns, and two calls differ.
    #[test]
    fn a_dead_address_stays_dead_and_distinct() {
        let (a, b) = (dead_addr(), dead_addr());
        assert_ne!(a, b);
        for addr in [a, b] {
            assert!(is_taken(addr), "{addr} was taken");
            assert!(TcpStream::connect(addr).is_err(), "{addr} answered");
        }
    }

    /// Catches a connected holder (the macOS one, run here too) that can be taken or
    /// that does not refuse: same assertions as for the Linux holder.
    #[test]
    fn a_connected_socket_holds_its_port_and_refuses() {
        let held = connected_socket(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("connect to a peer");
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, local_port(&held.fd)));
        assert!(is_taken(addr), "a thief took the port");
        let err = TcpStream::connect(addr).expect_err("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "{err}");
        drop(held);
    }

    /// Catches a probed port on the usual loopback address, which another test's
    /// `127.0.0.1:0` bind can take (Linux; elsewhere only `127.0.0.1` exists).
    #[cfg(target_os = "linux")]
    #[test]
    fn an_own_address_is_not_the_usual_loopback_and_is_stable() {
        let a = own_loopback_port();
        let b = own_loopback_port();
        assert_ne!(a.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
        assert_ne!(a.ip(), b.ip());
        // The same port number is free on 127.0.0.1 for a thief and the address is
        // still ours to bind.
        let _thief = TcpListener::bind((Ipv4Addr::LOCALHOST, a.port()));
        TcpListener::bind(a).expect("our address is still free");
    }
}
