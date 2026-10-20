//! A small blocking HTTP/1.1 client for the calls the proxy side of a host makes to the
//! ports of a child on the same host (its admin port and its operator port), so that
//! the admin calls, the fleet reads and the operator posts of the update path share one
//! client with one set of limits.
//!
//! What it promises:
//! - **Loopback only.** [`Client::new`] refuses an address that is not a loopback address
//!   (the whole 127/8 block, `::1`, and an IPv4-mapped form of either), so a mistaken port table
//!   cannot send the proxy secret or an operator token off the host.
//! - **One deadline per call**, for connect, write and read together. Every socket
//!   operation is given only what is left of the call's budget, so a peer that trickles
//!   bytes cannot keep a call alive past it; a timeout per read would not stop that.
//!   A deadline over an hour is cut to an hour.
//! - **A bounded answer.** The body is refused up front when its `Content-Length` is over
//!   [`Limits::max_body`]; the head (status line and headers) has a fixed limit. Bytes
//!   past the announced length are ignored.
//! - **The caller names the headers**, as [`Headers`]: the proxy secret, a bearer token,
//!   `Content-Type`. The client writes `Host`, `Connection: close` and `Content-Length`
//!   itself, and never sends `Origin`: those names cannot be set. A header value cannot
//!   carry a line break, so a token cannot add a header of its own.
//! - **Typed errors, no panics.** [`HttpError`] separates a refused connection, a
//!   timeout, a body over the limit and an answer that is not HTTP/1.1; a non-2xx status
//!   comes back as a [`Response`], and [`Response::success`] turns it into
//!   [`HttpError::Status`] with the body, which a child's refusals use to say why.
//!
//! What it does not do: TLS, redirects, keep-alive (one connection per call), retries,
//! `Transfer-Encoding` (a chunked answer is refused as [`HttpError::Unsupported`]; a
//! child's JSON answers carry a `Content-Length`), and response headers (none is returned
//! yet). A caller that needs a retry on `503` with `Retry-After` adds the header read here.
//! A request whose write fails (a child that answered and closed before it read a large
//! body) is reported as the failed write, without reading the answer.
//!
//! For callers: a call blocks its thread for up to the deadline, so a caller on an async
//! runtime makes it with `spawn_blocking` or on a thread of its own, never on a worker.
//! The limits belong to the [`Client`] and a call has no override of its own: a call that
//! needs a longer deadline than the rest uses a second client for the same address.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The most a status line and headers may take. A child's head is a few hundred bytes.
const MAX_HEAD: usize = 16 * 1024;

/// The most header lines read from an answer.
const MAX_HEADERS: usize = 64;

/// The longest deadline honoured: a call to a port on this host that needs more is stuck.
const MAX_DEADLINE: Duration = Duration::from_secs(3600);

/// Header names the client writes itself, or must never send. `Origin` is the browser's
/// proof of where a request came from; this client is not a browser, and a child that
/// reads it would treat the call as one.
const RESERVED: [&str; 5] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "origin",
];

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("{0} is not a loopback address")]
    NotLoopback(SocketAddr),
    #[error("the request cannot be sent: {0}")]
    BadRequest(&'static str),
    #[error("nothing accepted the connection")]
    Refused,
    #[error("the call did not finish within its deadline")]
    Timeout,
    #[error("the answer's body is over the limit of {limit} bytes")]
    TooLarge { limit: usize },
    #[error("malformed answer: {0}")]
    Malformed(&'static str),
    #[error("unsupported answer: {0}")]
    Unsupported(&'static str),
    #[error("i/o error: {0}")]
    Io(io::ErrorKind),
    /// A status outside 2xx, with its body, from [`Response::success`] and the JSON calls.
    #[error("the child answered with status {code}")]
    Status { code: u16, body: Vec<u8> },
    #[error("json: {0}")]
    Json(String),
}

/// The limits of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The whole call: connect, write the request, read the answer.
    pub deadline: Duration,
    /// The most body bytes accepted in the answer.
    pub max_body: usize,
}

/// Header lines the caller adds to a request. Values are secrets, so [`fmt::Debug`] shows
/// only a size.
#[derive(Clone, Default)]
pub struct Headers(Vec<u8>);

impl Headers {
    pub fn new() -> Headers {
        Headers::default()
    }

    /// `Content-Type: application/json`, which a child's JSON writes require.
    pub fn json() -> Headers {
        Headers(b"Content-Type: application/json\r\n".to_vec())
    }

    /// These headers, then `other`'s.
    pub fn and(mut self, other: &Headers) -> Headers {
        self.0.extend_from_slice(&other.0);
        self
    }

    /// Add `name: value`. Refused: an empty name or one with a character a header name
    /// cannot have, a name the client owns (see the module doc), and a value with
    /// anything but printable ASCII, space and tab (a CR or LF would end the header).
    pub fn with(mut self, name: &str, value: &[u8]) -> Result<Headers, HttpError> {
        if name.is_empty() || !name.bytes().all(is_name_byte) {
            return Err(HttpError::BadRequest("a header name is not a token"));
        }
        if RESERVED.iter().any(|r| name.eq_ignore_ascii_case(r)) {
            return Err(HttpError::BadRequest("the client owns this header name"));
        }
        if !value
            .iter()
            .all(|&b| b == b'\t' || (0x20..0x7f).contains(&b))
        {
            return Err(HttpError::BadRequest(
                "a header value has a control or non-ASCII byte",
            ));
        }
        self.0.extend_from_slice(name.as_bytes());
        self.0.extend_from_slice(b": ");
        self.0.extend_from_slice(value);
        self.0.extend_from_slice(b"\r\n");
        Ok(self)
    }
}

impl fmt::Debug for Headers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Headers")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// A character of a header name (RFC 9110 `token`).
fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// An answer: its status and body, whatever the status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    /// The body of a 2xx answer; any other status is [`HttpError::Status`] with its body.
    pub fn success(self) -> Result<Vec<u8>, HttpError> {
        if (200..300).contains(&self.status) {
            Ok(self.body)
        } else {
            Err(HttpError::Status {
                code: self.status,
                body: self.body,
            })
        }
    }
}

/// Calls to one address on this host. Cheap to make: a call opens its own connection.
#[derive(Debug, Clone, Copy)]
pub struct Client {
    addr: SocketAddr,
    limits: Limits,
}

impl Client {
    /// A client for `addr`, which must be a loopback address.
    pub fn new(addr: SocketAddr, limits: Limits) -> Result<Client, HttpError> {
        if addr.ip().to_canonical().is_loopback() {
            Ok(Client { addr, limits })
        } else {
            Err(HttpError::NotLoopback(addr))
        }
    }

    /// `GET path`. `path` starts with `/` and is already percent-encoded.
    pub fn get(&self, path: &str, headers: &Headers) -> Result<Response, HttpError> {
        self.exchange("GET", path, headers, None)
    }

    /// `POST path` with `body`, which the headers describe (`Content-Type`).
    pub fn post(&self, path: &str, headers: &Headers, body: &[u8]) -> Result<Response, HttpError> {
        self.exchange("POST", path, headers, Some(body))
    }

    /// `GET path`, and the body of the 2xx answer read as JSON.
    pub fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        headers: &Headers,
    ) -> Result<T, HttpError> {
        from_json(self.get(path, headers)?)
    }

    /// `POST path` with `body` as JSON (the caller sends `Content-Type`), and the body of
    /// the 2xx answer read as JSON.
    pub fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        headers: &Headers,
        body: &B,
    ) -> Result<T, HttpError> {
        let body = serde_json::to_vec(body).map_err(|e| HttpError::Json(e.to_string()))?;
        from_json(self.post(path, headers, &body)?)
    }

    fn exchange(
        &self,
        method: &str,
        path: &str,
        headers: &Headers,
        body: Option<&[u8]>,
    ) -> Result<Response, HttpError> {
        if !path.starts_with('/') || !path.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(HttpError::BadRequest("the path is not an absolute path"));
        }
        let (length, body) = match body {
            Some(body) => (format!("Content-Length: {}\r\n", body.len()), body),
            None => (String::new(), &[][..]),
        };
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{length}",
            self.addr
        )
        .into_bytes();
        request.extend_from_slice(&headers.0);
        request.extend_from_slice(b"\r\n");
        request.extend_from_slice(body);

        let deadline = Deadline::after(self.limits.deadline);
        let stream = deadline
            .left()
            .and_then(|left| TcpStream::connect_timeout(&self.addr, left))
            .map_err(|e| io_error(&e))?;
        let mut timed = Timed {
            stream: &stream,
            deadline,
        };
        timed
            .write_all(&request)
            .and_then(|()| timed.flush())
            .map_err(|e| io_error(&e))?;
        let mut reader = BufReader::new(timed);
        let head = read_head(&mut reader)?;
        let (status, length) = parse_head(&head, self.limits.max_body)?;
        let mut body = Vec::new();
        reader
            .take(length as u64)
            .read_to_end(&mut body)
            .map_err(|e| io_error(&e))?;
        if body.len() < length {
            return Err(HttpError::Malformed(
                "the connection closed inside the body",
            ));
        }
        Ok(Response { status, body })
    }
}

fn from_json<T: DeserializeOwned>(response: Response) -> Result<T, HttpError> {
    serde_json::from_slice(&response.success()?).map_err(|e| HttpError::Json(e.to_string()))
}

/// The end of one call's budget.
#[derive(Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    fn after(budget: Duration) -> Deadline {
        Deadline(Instant::now() + budget.min(MAX_DEADLINE))
    }

    /// What is left, as the timeout of the next socket operation; `TimedOut` when it is
    /// gone (a socket timeout of zero would mean "wait for ever").
    fn left(self) -> io::Result<Duration> {
        let left = self.0.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(left)
        }
    }
}

/// A socket whose every read and write is bounded by what is left of the call's deadline.
struct Timed<'a> {
    stream: &'a TcpStream,
    deadline: Deadline,
}

impl Read for Timed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.deadline.left()?))?;
        self.stream.read(buf)
    }
}

impl Write for Timed<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.deadline.left()?))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// A failed socket operation as the caller sees it. The sockets are blocking with a
/// timeout, so `WouldBlock` is what a read that timed out reports on Linux.
fn io_error(e: &io::Error) -> HttpError {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => HttpError::Timeout,
        io::ErrorKind::ConnectionRefused => HttpError::Refused,
        kind => HttpError::Io(kind),
    }
}

/// The status line and headers, through the blank line that ends them.
fn read_head(reader: &mut BufReader<Timed<'_>>) -> Result<Vec<u8>, HttpError> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let room = (MAX_HEAD + 1 - head.len()) as u64;
        let n = reader
            .by_ref()
            .take(room)
            .read_until(b'\n', &mut head)
            .map_err(|e| io_error(&e))?;
        if n == 0 {
            return Err(HttpError::Malformed(
                "the connection closed before the headers ended",
            ));
        }
        if head.len() > MAX_HEAD {
            return Err(HttpError::Malformed("the headers are too long"));
        }
    }
    Ok(head)
}

/// The status of an answer and the number of body bytes that follow its head.
fn parse_head(head: &[u8], max_body: usize) -> Result<(u16, usize), HttpError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Response::new(&mut headers);
    match parsed.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err(HttpError::Malformed("not a status line and headers")),
    }
    if parsed.version != Some(1) {
        return Err(HttpError::Malformed("not HTTP/1.1"));
    }
    let status = parsed.code.unwrap_or_default();
    if !(200..600).contains(&status) {
        return Err(HttpError::Malformed("no final status"));
    }
    let mut length = None;
    for header in parsed.headers.iter() {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(HttpError::Unsupported(
                "Transfer-Encoding (only Content-Length is read)",
            ));
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            let n = digits(header.value)
                .ok_or(HttpError::Malformed("Content-Length is not a number"))?;
            if length.replace(n).is_some_and(|before| before != n) {
                return Err(HttpError::Malformed(
                    "Content-Length is given twice, differently",
                ));
            }
        }
    }
    let length = if matches!(status, 204 | 304) {
        0
    } else {
        length.ok_or(HttpError::Malformed("the answer has no Content-Length"))?
    };
    if length > max_body {
        return Err(HttpError::TooLarge { limit: max_body });
    }
    Ok((status, length))
}

/// A decimal number of ASCII digits, and nothing else: no sign, no space, no overflow.
fn digits(value: &[u8]) -> Option<usize> {
    if value.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for &b in value {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    Some(n)
}
