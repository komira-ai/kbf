//! The download client over an in-memory connection: no socket, no name lookup.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};

use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};

use super::{Download, DownloadError, Net};

/// Answers every name with one documentation address; nothing is looked up.
#[derive(Debug)]
struct NoLookup;

impl Resolver for NoLookup {
    fn resolve(
        &self,
        _uri: &Uri,
        _config: &Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut addrs = self.empty();
        addrs.push(SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 80)));
        Ok(addrs)
    }
}

/// Opens an [`InMemory`] connection for every request.
#[derive(Debug)]
struct Server;

impl Connector for Server {
    type Out = InMemory;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<InMemory>, ureq::Error> {
        let c = details.config;
        Ok(Some(InMemory {
            buffers: LazyBuffers::new(c.input_buffer_size(), c.output_buffer_size()),
            request: Vec::new(),
            reply: Vec::new(),
            sent: 0,
        }))
    }
}

/// One connection: collects the request and, once its head is complete, serves the answer
/// for its path.
struct InMemory {
    buffers: LazyBuffers,
    request: Vec<u8>,
    reply: Vec<u8>,
    sent: usize,
}

impl fmt::Debug for InMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InMemory")
    }
}

/// The answer to `request` (a whole request head): `/ok` is 200 with `hello`, `/redirect`
/// is a 302 to `/ok`, `/big` is 200 with 1000 bytes, anything else is 404.
fn answer(request: &[u8]) -> Vec<u8> {
    let head = String::from_utf8_lossy(request);
    let path = head.split(' ').nth(1).unwrap_or_default();
    let (status, body) = match path {
        "/ok" => ("200 OK", "hello".to_owned()),
        "/redirect" => ("302 Found\r\nLocation: /ok", String::new()),
        "/big" => ("200 OK", "x".repeat(1000)),
        _ => ("404 Not Found", "nope".to_owned()),
    };
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

impl Transport for InMemory {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, _timeout: NextTimeout) -> Result<(), ureq::Error> {
        let sent = self.buffers.output()[..amount].to_vec();
        self.request.extend_from_slice(&sent);
        if self.request.ends_with(b"\r\n\r\n") {
            self.reply = answer(&self.request);
        }
        Ok(())
    }

    fn await_input(&mut self, _timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let room = self.buffers.input_append_buf();
        let n = room.len().min(self.reply.len() - self.sent);
        room[..n].copy_from_slice(&self.reply[self.sent..self.sent + n]);
        self.sent += n;
        self.buffers.input_appended(n);
        Ok(n > 0)
    }

    fn is_open(&mut self) -> bool {
        true
    }
}

/// Catches a client that treats an error status as a body, ignores the size limit, does not
/// follow a redirect (release assets are served from one), or dials plain http when it must
/// not; each failure comes back as its own kind of [`DownloadError`].
#[test]
fn downloads_follow_redirects_and_refuse_errors_big_bodies_and_plain_http() {
    let net = Net::with_parts(false, Server, NoLookup);
    let base = "http://downloads.example.com";
    assert_eq!(net.get(&format!("{base}/ok"), 100).unwrap(), b"hello");
    assert_eq!(net.get(&format!("{base}/redirect"), 100).unwrap(), b"hello");
    let missing = net.get(&format!("{base}/missing"), 100).unwrap_err();
    assert!(matches!(missing, DownloadError::Status(404)), "{missing:?}");
    assert_eq!(missing.to_string(), "answered 404");
    let big = net.get(&format!("{base}/big"), 10).unwrap_err();
    assert!(
        matches!(
            big,
            DownloadError::Transport(ureq::Error::BodyExceedsLimit(10))
        ),
        "{big:?}"
    );
    assert_eq!(net.get(&format!("{base}/big"), 2000).unwrap().len(), 1000);

    let strict = Net::with_parts(true, Server, NoLookup);
    let plain = strict.get(&format!("{base}/ok"), 100).unwrap_err();
    assert!(
        matches!(
            plain,
            DownloadError::Transport(ureq::Error::RequireHttpsOnly(_))
        ),
        "{plain:?}"
    );
}

/// Catches `Net::new` building something other than a working client over the network's own
/// connector: refused before any connection, a plain-http URL fails the same way.
#[test]
fn the_network_client_refuses_plain_http() {
    let err = Net::new(true)
        .get("http://downloads.example.com/ok", 100)
        .unwrap_err();
    assert!(
        matches!(
            err,
            DownloadError::Transport(ureq::Error::RequireHttpsOnly(_))
        ),
        "{err:?}"
    );
}
