//! The operator API as `deploy` uses it: one request, one reply, behind a trait so a test
//! plays the server. [`Http`] is the real client: HTTP/1.1 to a loopback address, one
//! request per connection (`Connection: close`), with the bearer token on every request.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

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

    pub fn post_json(path: &str, body: &serde_json::Value) -> Request {
        Request {
            method: "POST",
            path: path.to_owned(),
            body: body.to_string().into_bytes(),
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

/// The real client.
pub struct Http {
    addr: SocketAddr,
    token: String,
    timeout: Duration,
}

impl Http {
    /// `addr` must be a loopback address: the token is sent in clear, and the server
    /// refuses a write from anywhere else.
    pub fn new(addr: SocketAddr, token: String, timeout: Duration) -> io::Result<Http> {
        if !addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the operator API address must be loopback",
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
        stream.write_all(head.as_bytes())?;
        stream.write_all(&request.body)?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        parse_reply(&raw)
    }
}

/// Parse a whole `Connection: close` answer, with a plain or a chunked body.
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
    let chunked = lines.any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = &raw[split + 4..];
    let body = if chunked {
        dechunk(body).ok_or_else(|| bad("a bad chunked body"))?
    } else {
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
