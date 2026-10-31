//! The addresses a front listens on: more than one of them, bound one socket each.
//!
//! A packet that an `nft` redirect sends to a private port arrives with the address of the
//! interface it came in on, so an instance listens on the wildcard of both families, `0.0.0.0:P`
//! and `[::]:P`. They can only be two sockets, and the IPv6 one must say `IPV6_V6ONLY`, or the
//! system lets it take the IPv4 packets too and the second bind is refused.
//!
//! [`Addrs`] is what the `listen` keys of the file are read into (one address, or a list), and
//! [`bind_all`] and [`Listeners`] are the sockets of a front: one server, one drain and one limit
//! on connections serve them all.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{AddressFamily, SocketType, bind as bind_socket, listen, socket, sockopt};
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::net::{TcpListener, TcpStream};

use super::Incoming;

/// The value of a `listen` key of the file: one address, or a list of them. It is written as a
/// string when it is one address, so a file that has one reads and prints as it always did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Addrs(pub Vec<SocketAddr>);

impl Serialize for Addrs {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [only] => only.serialize(serializer),
            several => several.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Addrs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Reader;

        impl<'de> Visitor<'de> for Reader {
            type Value = Addrs;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a socket address, or a list of them")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Addrs, E> {
                Ok(Addrs(vec![address(text)?]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Addrs, A::Error> {
                let mut addrs = Vec::new();
                while let Some(text) = seq.next_element::<String>()? {
                    addrs.push(address(&text)?);
                }
                Ok(Addrs(addrs))
            }
        }

        deserializer.deserialize_any(Reader)
    }
}

/// The address `text` spells. The message holds no part of the text.
fn address<E: de::Error>(text: &str) -> Result<SocketAddr, E> {
    text.parse()
        .map_err(|_| E::custom("invalid socket address syntax"))
}

/// Whether the same address is in `addrs` twice.
pub(super) fn has_duplicate(addrs: &[SocketAddr]) -> bool {
    addrs
        .iter()
        .enumerate()
        .any(|(at, addr)| addrs[..at].contains(addr))
}

/// The length of the queue of connections the system keeps for a socket that is not yet
/// accepted from.
const BACKLOG: i32 = 1024;

/// A listener on `addr`, which must be made inside the runtime. An IPv6 address is bound for
/// IPv6 only (`IPV6_V6ONLY`, set explicitly: the system's default is not the same everywhere),
/// so that `[::]:P` leaves `0.0.0.0:P` to a socket of its own.
///
/// # Errors
/// The address cannot be bound.
pub(crate) fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let family = match addr {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    };
    // The close-on-exec flag is set after the socket is made, not by the call that makes it: that
    // call takes the flag on Linux only, and this code is built for macOS as well.
    let socket = socket(family, SocketType::STREAM, None)?;
    fcntl_setfd(&socket, FdFlags::CLOEXEC)?;
    // As `std` does for a listener: a restart does not wait for the connections of the last run.
    sockopt::set_socket_reuseaddr(&socket, true)?;
    if addr.is_ipv6() {
        sockopt::set_ipv6_v6only(&socket, true)?;
    }
    bind_socket(&socket, &addr)?;
    listen(&socket, BACKLOG)?;
    let listener = std::net::TcpListener::from(socket);
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

/// A listener on each of `addrs`, in the order given, or the first that cannot be bound with
/// what the system said.
///
/// # Errors
/// The address that cannot be bound, and why.
pub(crate) fn bind_all(addrs: &[SocketAddr]) -> Result<Vec<TcpListener>, (SocketAddr, io::Error)> {
    let mut bound = Vec::new();
    for addr in addrs {
        bound.push(bind(*addr).map_err(|why| (*addr, why))?);
    }
    Ok(bound)
}

/// The address of each of `listeners`, in order.
///
/// # Errors
/// A socket's address cannot be read.
pub(crate) fn local_addrs(listeners: &[TcpListener]) -> io::Result<Vec<SocketAddr>> {
    listeners.iter().map(TcpListener::local_addr).collect()
}

/// The addresses as the start line shows them: separated by commas, with no space, so that the
/// line is still words of `name=value`.
pub(crate) fn joined(addrs: &[SocketAddr]) -> String {
    let words: Vec<String> = addrs.iter().map(ToString::to_string).collect();
    words.join(",")
}

/// The sockets of a front, taken as one source of connections.
pub(crate) struct Listeners {
    listeners: Vec<TcpListener>,
    /// The socket the next ask begins at.
    next: usize,
}

impl Listeners {
    /// The source of the connections of `listeners`.
    pub(crate) fn new(listeners: Vec<TcpListener>) -> Self {
        Self { listeners, next: 0 }
    }

    /// The address of each socket, in the order they were given.
    pub(crate) fn local_addrs(&self) -> io::Result<Vec<SocketAddr>> {
        local_addrs(&self.listeners)
    }
}

impl Incoming for Listeners {
    type Stream = TcpStream;

    /// Takes the next connection of any of the sockets. The ask begins at the socket after the
    /// one that was last taken from, so that a busy socket does not leave another unserved; a
    /// socket that has none registers the waker, which any of them wakes.
    fn poll_incoming(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        let count = self.listeners.len();
        for step in 0..count {
            let at = (self.next + step) % count;
            if let Poll::Ready(accepted) = self.listeners[at].poll_accept(cx) {
                self.next = (at + 1) % count;
                return Poll::Ready(accepted);
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
#[path = "listen_tests.rs"]
mod tests;
