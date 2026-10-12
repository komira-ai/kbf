//! A small HTTPS client for the kits: one `GET` per connection over rustls (the ring
//! provider), with the server checked against the host's trusted roots.
//!
//! It reads the status line and headers (`httparse`), then the body by
//! `Transfer-Encoding: chunked`, by `Content-Length`, or to the end of the connection.
//! Only a 2xx body is written out; any other is read and dropped. [`Client::follow`]
//! follows up to [`REDIRECTS`] redirects, each to another `https://` URL.
//!
//! Not reqwest: its rustls feature brings the aws-lc-rs provider, a C library under the
//! OpenSSL licence, which `deny.toml` refuses; and its no-provider feature, which cargo
//! would turn on for every crate in the workspace, makes every other reqwest client
//! panic when built without a process-wide provider.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

/// The most redirects [`Client::follow`] takes before it gives up.
pub const REDIRECTS: usize = 5;
/// The most bytes of a status line and headers read.
const HEAD_MAX: usize = 64 * 1024;
/// The most bytes of a body [`read_reply`] keeps.
const REPLY_MAX: usize = 1024 * 1024;

/// An `https://` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpsUrl {
    url: String,
    authority: String,
    host: String,
    port: u16,
    path: String,
}

/// Why a string is not an [`HttpsUrl`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpsUrlError {
    #[error("{0:?} is not an https:// URL")]
    Scheme(String),
    #[error("{0:?} names no host, or names a user")]
    Host(String),
    #[error("{0:?} has a port that is not a number from 1 to 65535")]
    Port(String),
}

impl HttpsUrl {
    pub fn new(url: &str) -> Result<HttpsUrl, HttpsUrlError> {
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(HttpsUrlError::Scheme(url.to_owned()));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        // No user part (`https://user:key@host/`): it would be printed with the URL.
        if authority.contains(['@', '[', ']']) {
            return Err(HttpsUrlError::Host(url.to_owned()));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port = port.parse::<u16>().ok().filter(|p| *p != 0);
                let Some(port) = port else {
                    return Err(HttpsUrlError::Port(url.to_owned()));
                };
                (host, port)
            }
            None => (authority, 443),
        };
        if host.is_empty() {
            return Err(HttpsUrlError::Host(url.to_owned()));
        }
        Ok(HttpsUrl {
            url: url.to_owned(),
            authority: authority.to_owned(),
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path and query sent in the request line, `/` when the URL has none.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The last part of the path, without the query.
    pub fn file_name(&self) -> &str {
        let path = self.path.split('?').next().unwrap_or_default();
        path.rsplit('/').next().unwrap_or_default()
    }
}

impl fmt::Display for HttpsUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}

/// Why a `GET` failed.
#[derive(Debug, thiserror::Error)]
pub enum HttpsError {
    #[error("the host has no trusted root certificates to check servers against")]
    NoRoots,
    #[error("{url}: {source}")]
    Io { url: String, source: io::Error },
    #[error("{url}: the answer is not HTTP: {reason}")]
    Malformed { url: String, reason: String },
    #[error("{url}: a redirect to {to:?}, which is not an https:// URL")]
    Redirect { url: String, to: String },
    #[error("{url}: more than {REDIRECTS} redirects")]
    Redirects { url: String },
    #[error("{url}: the answer's body is longer than {REPLY_MAX} bytes")]
    TooLong { url: String },
}

/// What a `GET` was answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    /// The `Location` header, if any.
    pub location: Option<String>,
}

/// The client: the roots it trusts and how long a connect, read or write may take.
#[derive(Clone, Debug)]
pub struct Client {
    config: Arc<ClientConfig>,
    timeout: Duration,
}

impl Client {
    /// A client trusting the host's root certificates (the system store, or the file
    /// `SSL_CERT_FILE` names).
    pub fn system(timeout: Duration) -> Result<Client, HttpsError> {
        let found = rustls_native_certs::load_native_certs();
        Ok(Client::with_roots(roots(found.certs)?, timeout))
    }

    /// A client trusting only `roots`.
    pub fn with_roots(roots: RootCertStore, timeout: Duration) -> Client {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Client {
            config: Arc::new(config),
            timeout,
        }
    }

    /// One `GET` of `url`, connecting to `addr` when given (instead of resolving the
    /// URL's host; the server must still prove the URL's name). A 2xx body goes to
    /// `body`.
    pub fn get(
        &self,
        url: &HttpsUrl,
        addr: Option<SocketAddr>,
        body: &mut dyn Write,
    ) -> Result<Answer, HttpsError> {
        self.get_with(url, addr, &[], body)
    }

    /// [`get`](Client::get) with `headers` added to the request (an API's key, say:
    /// headers are never printed).
    pub fn get_with(
        &self,
        url: &HttpsUrl,
        addr: Option<SocketAddr>,
        headers: &[(&str, &str)],
        body: &mut dyn Write,
    ) -> Result<Answer, HttpsError> {
        let io = |source| HttpsError::Io {
            url: url.to_string(),
            source,
        };
        let addr = match addr {
            Some(a) => a,
            None => resolve(url).map_err(io)?,
        };
        let tcp = TcpStream::connect_timeout(&addr, self.timeout).map_err(io)?;
        tcp.set_read_timeout(Some(self.timeout)).map_err(io)?;
        tcp.set_write_timeout(Some(self.timeout)).map_err(io)?;
        let name =
            ServerName::try_from(url.host().to_owned()).map_err(|e| HttpsError::Malformed {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
        let conn = ClientConnection::new(Arc::clone(&self.config), name)
            .map_err(io::Error::other)
            .map_err(io)?;
        let mut tls = StreamOwned::new(conn, tcp);
        let mut request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: kbf-deploy\r\nAccept: */*\r\nConnection: close\r\n",
            url.path(),
            url.authority
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        tls.write_all(request.as_bytes()).map_err(io)?;
        tls.flush().map_err(io)?;
        read_answer(url, BufReader::new(tls), body)
    }

    /// [`get`](Client::get), following redirects to `https://` URLs; the final URL's
    /// answer.
    pub fn follow(&self, url: &HttpsUrl, body: &mut dyn Write) -> Result<Answer, HttpsError> {
        let mut at = url.clone();
        for _ in 0..=REDIRECTS {
            let answer = self.get(&at, None, body)?;
            let Some(to) = answer
                .location
                .clone()
                .filter(|_| is_redirect(answer.status))
            else {
                return Ok(answer);
            };
            at = HttpsUrl::new(&to).map_err(|_| HttpsError::Redirect {
                url: at.to_string(),
                to,
            })?;
        }
        Err(HttpsError::Redirects {
            url: url.to_string(),
        })
    }
}

/// A root store of `certs`; refused when none parses.
pub fn roots(certs: Vec<CertificateDer<'static>>) -> Result<RootCertStore, HttpsError> {
    let mut store = RootCertStore::empty();
    store.add_parsable_certificates(certs);
    if store.is_empty() {
        return Err(HttpsError::NoRoots);
    }
    Ok(store)
}

fn resolve(url: &HttpsUrl) -> io::Result<SocketAddr> {
    (url.host(), url.port())
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::from(io::ErrorKind::AddrNotAvailable))
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The status line and headers of one answer, as the body is read by.
struct Head {
    status: u16,
    chunked: bool,
    length: Option<u64>,
    location: Option<String>,
}

/// The status, headers and body of one answer from `r`; only a 2xx body goes to `body`.
fn read_answer<R: BufRead>(
    url: &dyn fmt::Display,
    mut r: R,
    body: &mut dyn Write,
) -> Result<Answer, HttpsError> {
    let head = read_head(url, &mut r)?;
    let mut dropped = io::sink();
    let to: &mut dyn Write = if (200..300).contains(&head.status) {
        body
    } else {
        &mut dropped
    };
    read_body(url, &head, &mut r, to)?;
    Ok(Answer {
        status: head.status,
        location: head.location,
    })
}

/// The status of one answer from `r` and its body, whatever the status: an API's
/// refusal says why in its body. A body longer than [`REPLY_MAX`] is an error, however
/// it is framed.
pub(crate) fn read_reply<R: BufRead>(
    url: &dyn fmt::Display,
    mut r: R,
) -> Result<(u16, Vec<u8>), HttpsError> {
    let head = read_head(url, &mut r)?;
    let mut body = Capped {
        bytes: Vec::new(),
        over: false,
    };
    let got = read_body(url, &head, &mut r, &mut body);
    if body.over {
        return Err(HttpsError::TooLong {
            url: url.to_string(),
        });
    }
    got?;
    Ok((head.status, body.bytes))
}

/// A body kept in memory that refuses to grow past [`REPLY_MAX`] and says it did.
struct Capped {
    bytes: Vec<u8>,
    over: bool,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + buf.len() > REPLY_MAX {
            self.over = true;
            return Err(io::Error::other("the body is too long"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_head<R: BufRead>(url: &dyn fmt::Display, r: &mut R) -> Result<Head, HttpsError> {
    let io = io_error(url);
    let malformed = |reason: String| HttpsError::Malformed {
        url: url.to_string(),
        reason,
    };
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let n = r
            .by_ref()
            .take(1 + (HEAD_MAX - head.len()) as u64)
            .read_until(b'\n', &mut head)
            .map_err(&io)?;
        if n == 0 || head.len() > HEAD_MAX {
            return Err(malformed(
                "the headers end early or are too long".to_owned(),
            ));
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers);
    parsed.parse(&head).map_err(|e| malformed(e.to_string()))?;
    let header = |name: &str| {
        parsed
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.value).trim().to_owned())
    };
    let chunked = header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let length = match header("content-length") {
        Some(length) => Some(
            length
                .parse()
                .map_err(|_| malformed(format!("Content-Length {length:?}")))?,
        ),
        None => None,
    };
    Ok(Head {
        status: parsed.code.unwrap_or_default(),
        chunked,
        length,
        location: header("location"),
    })
}

/// The error of a failed read of an answer from `url`.
fn io_error(url: &dyn fmt::Display) -> impl Fn(io::Error) -> HttpsError + '_ {
    move |source| HttpsError::Io {
        url: url.to_string(),
        source,
    }
}

/// The body after `head`: by its chunks, by its Content-Length, or to the end.
fn read_body<R: BufRead>(
    url: &dyn fmt::Display,
    head: &Head,
    r: &mut R,
    to: &mut dyn Write,
) -> Result<(), HttpsError> {
    let io = io_error(url);
    if head.chunked {
        read_chunked(r, to).map_err(&io)?;
    } else if let Some(n) = head.length {
        let copied = io::copy(&mut r.by_ref().take(n), to).map_err(&io)?;
        if copied != n {
            return Err(io(io::Error::from(io::ErrorKind::UnexpectedEof)));
        }
    } else {
        io::copy(r, to).map_err(&io)?;
    }
    Ok(())
}

/// A `Transfer-Encoding: chunked` body: `<hex size>[;ext]\r\n<bytes>\r\n`, up to a size
/// of 0, then trailer lines up to an empty one.
fn read_chunked<R: BufRead>(r: &mut R, to: &mut dyn Write) -> io::Result<()> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    loop {
        let line = read_line(r)?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size =
            u64::from_str_radix(size, 16).map_err(|_| bad("a chunk size that is not hex"))?;
        if size == 0 {
            while !read_line(r)?.is_empty() {}
            return Ok(());
        }
        if io::copy(&mut r.by_ref().take(size), to)? != size {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if !read_line(r)?.is_empty() {
            return Err(bad("a chunk longer than its size"));
        }
    }
}

/// One line without its `\r\n`; an error at the end of the input.
fn read_line<R: BufRead>(r: &mut R) -> io::Result<String> {
    let mut line = String::new();
    if r.by_ref().take(4096).read_line(&mut line)? == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
#[path = "https_tests.rs"]
mod tests;
