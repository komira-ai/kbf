//! The operator API as `deploy` uses it: one request, one reply, behind a trait so a test
//! plays the server. [`Https`] is the real client: HTTP/1.1 over TLS to the frontend's public
//! port, one request per connection (`Connection: close`), with the bearer token on every
//! request.
//!
//! The public port belongs to the proxy that fronts the frontend's children
//! (`docs/design/frontend-proxy.md`): it serves the update API itself and passes every other
//! `/v1/*` call (the release store, the node and member lists) to whichever child is active,
//! so every call of a deploy goes to one address that does not move when the children swap.
//! The proxy speaks TLS only, so the client verifies the certificate: under the name the
//! certificate carries (the address's own IP when none is given), against the system's roots
//! and the CA certificates of a PEM file, if one is given.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde::Serialize;

/// The most bytes of an answer read. The largest is the release list.
const REPLY_MAX: usize = 32 * 1024 * 1024;

/// One request. `body` is sent with `Content-Type: application/json` unless `raw` is set
/// (a release upload, whose body is the file's bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: &'static str,
    pub path: String,
    pub body: Vec<u8>,
    pub raw: bool,
}

impl Request {
    pub fn get(path: &str) -> Request {
        Request {
            method: "GET",
            path: path.to_owned(),
            body: Vec::new(),
            raw: false,
        }
    }

    pub fn post_json(path: &str, body: &impl Serialize) -> Request {
        Request {
            method: "POST",
            path: path.to_owned(),
            // The bodies are plain structs and maps: serialization cannot fail.
            body: serde_json::to_vec(body).unwrap_or_default(),
            raw: false,
        }
    }

    pub fn put_raw(path: &str, body: Vec<u8>) -> Request {
        Request {
            method: "PUT",
            path: path.to_owned(),
            body,
            raw: true,
        }
    }
}

/// An answer: the status and the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub trait Operator {
    /// Send `request`. An error is no answer (the connection failed or was cut).
    fn send(&self, request: &Request) -> io::Result<Reply>;
}

/// How the server's certificate is verified.
#[derive(Clone, Debug, Default)]
pub struct Trust {
    /// The DNS name the certificate carries. Without one the address's IP is verified.
    pub server_name: Option<String>,
    /// PEM of CA certificates trusted besides the system's roots.
    pub ca_pem: Option<Vec<u8>>,
}

/// The real client.
pub struct Https {
    addr: SocketAddr,
    name: ServerName<'static>,
    config: Arc<ClientConfig>,
    token: String,
    timeout: Duration,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_owned())
}

/// The roots the host trusts (the system store, or the file `SSL_CERT_FILE` names).
fn system_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// The certificates in `pem`; an error when there is none or one is not PEM.
fn pem_certs(pem: &[u8]) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    if certs.is_empty() {
        return Err(invalid("the CA file holds no PEM certificate"));
    }
    Ok(certs)
}

impl Https {
    /// `addr` must be a loopback address: a deploy runs on the frontend host and drives its
    /// own proxy, and a root-owned config naming another host is a mistake to stop.
    pub fn new(
        addr: SocketAddr,
        trust: &Trust,
        token: String,
        timeout: Duration,
    ) -> io::Result<Https> {
        Https::with_roots(addr, trust, system_roots(), token, timeout)
    }

    /// [`Https::new`] with the system's roots given.
    pub(super) fn with_roots(
        addr: SocketAddr,
        trust: &Trust,
        system: Vec<CertificateDer<'static>>,
        token: String,
        timeout: Duration,
    ) -> io::Result<Https> {
        if !addr.ip().to_canonical().is_loopback() {
            return Err(invalid("the operator API address must be loopback"));
        }
        let name = match &trust.server_name {
            Some(name) => ServerName::try_from(name.clone())
                .map_err(|_| invalid("the certificate name is not a DNS name or an IP"))?,
            None => ServerName::IpAddress(addr.ip().to_canonical().into()),
        };
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(system);
        let extra = match &trust.ca_pem {
            Some(pem) => pem_certs(pem)?,
            None => Vec::new(),
        };
        for cert in extra {
            roots.add(cert).map_err(io::Error::other)?;
        }
        if roots.is_empty() {
            return Err(invalid(
                "the host trusts no root certificate; give the farm's CA file",
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Https {
            addr,
            name,
            config: Arc::new(config),
            token,
            timeout,
        })
    }

    /// The head of the request, which the body follows.
    fn head(&self, request: &Request) -> String {
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n",
            request.method, request.path, self.addr, self.token
        );
        if request.method != "GET" {
            let kind = if request.raw {
                "application/octet-stream"
            } else {
                "application/json"
            };
            head.push_str(&format!(
                "Content-Type: {kind}\r\nContent-Length: {}\r\n",
                request.body.len()
            ));
        }
        head.push_str("\r\n");
        head
    }
}

impl Operator for Https {
    fn send(&self, request: &Request) -> io::Result<Reply> {
        let tcp = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        tcp.set_read_timeout(Some(self.timeout))?;
        tcp.set_write_timeout(Some(self.timeout))?;
        let conn = ClientConnection::new(Arc::clone(&self.config), self.name.clone())
            .map_err(io::Error::other)?;
        let mut tls = StreamOwned::new(conn, tcp);
        // The body is a release component, which may be large: it is not copied.
        tls.write_all(self.head(request).as_bytes())?;
        tls.write_all(&request.body)?;
        tls.flush()?;
        let raw = read_all(&mut tls, REPLY_MAX);
        tls.conn.send_close_notify();
        // The answer is in hand: whether the goodbye is delivered does not matter.
        let _ = tls.flush();
        parse_reply(&raw?)
    }
}

/// Everything `r` gives, up to `max` bytes. A peer that closes the TCP connection without
/// the TLS goodbye ends the answer; its length is checked against `Content-Length` by
/// [`parse_reply`]. An answer longer than `max` is an error.
pub(super) fn read_all(r: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    match r.take(max as u64 + 1).read_to_end(&mut raw) {
        Err(e) if e.kind() != io::ErrorKind::UnexpectedEof => return Err(e),
        _ => {}
    }
    if raw.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the answer is longer than {max} bytes"),
        ));
    }
    Ok(raw)
}

/// Parse a whole `Connection: close` answer, with a plain or a chunked body. A body that is
/// not as long as `Content-Length` says is a cut connection, not an answer.
pub fn parse_reply(raw: &[u8]) -> io::Result<Reply> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no end of headers"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad("headers are not text"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad("no status"))?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let line = line.to_ascii_lowercase();
        chunked |= line.starts_with("transfer-encoding:") && line.contains("chunked");
        if let Some(value) = line.strip_prefix("content-length:") {
            let n = value.trim().parse::<usize>();
            length = Some(n.map_err(|_| bad("Content-Length is not a number"))?);
        }
    }
    let body = &raw[split + 4..];
    let body = if chunked {
        dechunk(body).ok_or_else(|| bad("a bad chunked body"))?
    } else {
        if length.is_some_and(|n| n != body.len()) {
            return Err(bad("the body is not as long as Content-Length says"));
        }
        body.to_vec()
    };
    Ok(Reply { status, body })
}

fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&rest[..eol]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
}
