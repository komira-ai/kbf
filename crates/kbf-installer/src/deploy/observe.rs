//! Which frontend the gates read, and how that follows a handover.
//!
//! On a host behind the frontend proxy the serving child changes with a handover: slot A
//! listens at `base_port`, slot B ten ports above, and the operator listener is at offset 2
//! of a slot (docs/design/frontend-proxy.md section 7; the offsets are the proxy's frozen
//! contract and are copied here, as this crate does not link the proxy). The proxy answers
//! `/metrics` itself and no `/v1` path carries the frontend's own series, so the gates
//! read the child's operator listener directly, and a deploy that hands the frontend over
//! must read the new child afterwards. [`Observe::resolve`] is called at the start and after
//! every frontend step.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use std::io::{self, Write};
use std::net::TcpStream;

use super::api::{Operator, Reply, Request, parse_reply, read_all};
// The port layout and the slot choice live in `crate::observe` (`setup` needs them without the
// deploy verb); they are re-exported so the observers here and their callers read as before.
pub use crate::observe::{
    MAX_BASE_PORT, OPERATOR_OFFSET, SLOT_STRIDE, Slot, choose, operator_port,
};

/// The most a reply from the observed frontend may be (a `/metrics` page, a listing).
const REPLY_MAX: usize = 16 * 1024 * 1024;

/// A plain HTTP/1.1 client to a loopback address: the frontend's children listen without TLS
/// behind the proxy, and only the proxy's public port (`--api`) speaks it. One request per
/// connection, the bearer token on each.
pub struct Http {
    addr: SocketAddr,
    token: String,
    timeout: Duration,
}

impl Http {
    /// `addr` must be loopback: the token is sent in the clear.
    pub fn new(addr: SocketAddr, token: String, timeout: Duration) -> io::Result<Http> {
        if !addr.ip().to_canonical().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the observed frontend's address must be loopback",
            ));
        }
        Ok(Http {
            addr,
            token,
            timeout,
        })
    }
}

impl Operator for Http {
    fn send(&self, request: &Request) -> io::Result<Reply> {
        let mut stream = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        // The observed frontend is only read from: a body is never sent.
        let head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            request.method, request.path, self.addr, self.token
        );
        stream.write_all(head.as_bytes())?;
        parse_reply(&read_all(&mut stream, REPLY_MAX)?)
    }
}

/// A frontend to read, and what to call it in the result document.
pub struct Observed<'a> {
    pub label: String,
    pub api: Box<dyn Operator + 'a>,
    /// Other children whose `/metrics` still count when they answer: the one that handed
    /// over keeps draining after the swap, and it is that one that reports
    /// `kbf_frontend_relay_untraced` (the successor never does).
    pub others: Vec<Other<'a>>,
}

pub struct Other<'a> {
    pub label: String,
    pub api: Box<dyn Operator + 'a>,
}

/// Finds the frontend the gates read.
pub trait Observe {
    /// The frontend serving now. `serving` is the slot the update API reports, when it
    /// reports one.
    fn resolve(&self, serving: Option<Slot>) -> Result<Observed<'_>, String>;
}

/// One fixed address (`--observe`): a frontend that does not move.
pub struct Fixed {
    pub addr: SocketAddr,
    pub token: String,
    pub timeout: Duration,
}

impl Observe for Fixed {
    fn resolve(&self, _: Option<Slot>) -> Result<Observed<'_>, String> {
        let api =
            Http::new(self.addr, self.token.clone(), self.timeout).map_err(|e| e.to_string())?;
        Ok(Observed {
            label: format!("fixed {}", self.addr),
            api: Box::new(api),
            others: Vec::new(),
        })
    }
}

/// No frontend to read: the verbs that run no gate (`update --frontend`). Asking it is an
/// error, never a guess.
pub struct NoObserve;

impl Observe for NoObserve {
    fn resolve(&self, _: Option<Slot>) -> Result<Observed<'_>, String> {
        Err("this verb observes no frontend".to_owned())
    }
}

/// The children of the frontend proxy (`--observe-base-port`): follows the serving slot.
pub struct Slots {
    pub base_port: u16,
    pub token: String,
    pub timeout: Duration,
}

impl Slots {
    fn client(&self, slot: Slot) -> Result<(SocketAddr, Http), String> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, operator_port(self.base_port, slot)));
        let http = Http::new(addr, self.token.clone(), self.timeout).map_err(|e| e.to_string())?;
        Ok((addr, http))
    }
}

impl Observe for Slots {
    fn resolve(&self, serving: Option<Slot>) -> Result<Observed<'_>, String> {
        let answers = |slot| {
            self.client(slot).is_ok_and(|(_, http)| {
                http.send(&Request::get("/metrics"))
                    .is_ok_and(|r| r.status == 200)
            })
        };
        let slot = choose(serving, answers)?;
        let (addr, http) = self.client(slot)?;
        let other = if slot == Slot::A { Slot::B } else { Slot::A };
        let others = self
            .client(other)
            .map(|(addr, http)| Other {
                label: format!("{other} {addr}"),
                api: Box::new(http) as Box<dyn Operator + '_>,
            })
            .into_iter()
            .collect();
        Ok(Observed {
            label: format!("{slot} {addr}"),
            api: Box::new(http),
            others,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the observer refuses an address that is not loopback (the token goes in the
    /// clear) and accepts both loopback spellings, an IPv4-mapped IPv6 loopback included.
    /// Catches the check dropped, or inverted.
    ///
    /// Mutants planted: the loopback test removed; `to_canonical` dropped (the mapped
    /// spelling is then refused).
    #[test]
    fn only_a_loopback_address_is_observed() {
        let t = Duration::from_secs(1);
        for addr in ["203.0.113.9:80", "[2001:db8::1]:80", "0.0.0.0:80"] {
            let e = Http::new(addr.parse().expect("addr"), "tok".to_owned(), t)
                .err()
                .expect("refused");
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{addr}");
            assert!(e.to_string().contains("must be loopback"), "{addr}: {e}");
        }
        for addr in ["127.0.0.1:80", "[::1]:80", "[::ffff:127.0.0.1]:80"] {
            assert!(
                Http::new(addr.parse().expect("addr"), "tok".to_owned(), t).is_ok(),
                "{addr}"
            );
        }
    }
}
