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
        match self.0.first() {
            Some(first) => first.serialize(serializer),
            None => self.0.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Addrs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Reader;

        impl<'de> Visitor<'de> for Reader {
            type Value = Addrs;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a socket address")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Addrs, E> {
                Ok(Addrs(vec![address(text)?]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, _: A) -> Result<Addrs, A::Error> {
                Err(de::Error::invalid_type(de::Unexpected::Seq, &self))
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
pub(super) fn has_duplicate(_addrs: &[SocketAddr]) -> bool {
    false
}

/// A listener on `addr`, which must be made inside the runtime.
///
/// # Errors
/// The address cannot be bound.
pub(crate) fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(addr)?;
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
    for addr in addrs.iter().take(1) {
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
}

impl Listeners {
    /// The source of the connections of `listeners`.
    pub(crate) fn new(mut listeners: Vec<TcpListener>) -> Self {
        listeners.truncate(1);
        Self { listeners }
    }

    /// The address of each socket, in the order they were given.
    pub(crate) fn local_addrs(&self) -> io::Result<Vec<SocketAddr>> {
        local_addrs(&self.listeners)
    }
}

impl Incoming for Listeners {
    type Stream = TcpStream;

    fn poll_incoming(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(TcpStream, SocketAddr)>> {
        match self.listeners.first() {
            Some(first) => first.poll_accept(cx),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
#[path = "listen_tests.rs"]
mod tests;
