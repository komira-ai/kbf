//! A frontend's operator API as this host reaches it: `GET /readyz` and `GET /healthz`
//! (`docs/api.md`), plain HTTP, both open reads.
//!
//! The port is one call, a `GET`. [`Http`] is the real one: one TCP connection per call, a
//! request in HTTP/1.0 (so that the answer is not chunked and ends with the connection), a
//! time limit on connecting and on the whole answer, and a limit on its size.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// An answer: its status and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// A `GET` of a frontend's operator API.
pub trait Probe {
    /// `GET path` at `addr`.
    ///
    /// # Errors
    /// No answer that is an HTTP response, in a line.
    fn get(&self, addr: SocketAddr, path: &str) -> Result<Reply, String>;
}

/// The most of an answer that is read.
const ANSWER_MAX: usize = 64 * 1024;

/// HTTP over a TCP connection, with `limit` for all of one call.
pub struct Http {
    pub limit: Duration,
}

impl Probe for Http {
    fn get(&self, addr: SocketAddr, path: &str) -> Result<Reply, String> {
        let end = Instant::now() + self.limit;
        let say = |e: std::io::Error| format!("{addr}{path}: {e}");
        let mut stream = TcpStream::connect_timeout(&addr, self.limit).map_err(say)?;
        stream.set_write_timeout(Some(self.limit)).map_err(say)?;
        let request = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).map_err(say)?;
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 4096];
        // Each read gets what is left of the time for the whole call.
        while raw.len() < ANSWER_MAX {
            // A time that is up is a read timeout of nothing, which the socket refuses.
            let left = end.saturating_duration_since(Instant::now());
            stream.set_read_timeout(Some(left)).map_err(say)?;
            let n = stream.read(&mut chunk).map_err(say)?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
        }
        parse(&raw).ok_or_else(|| format!("{addr}{path}: the answer is not an HTTP response"))
    }
}

/// The status and body of the bytes of an HTTP/1.x response.
fn parse(raw: &[u8]) -> Option<Reply> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let mut line = head.lines().next()?.split(' ');
    let (version, status) = (line.next()?, line.next()?);
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    Some(Reply {
        status: status.parse().ok()?,
        body: String::from_utf8_lossy(&raw[end + 4..]).into_owned(),
    })
}

/// Whether the frontend at `addr` says it is ready: `/readyz` answers 200.
pub fn ready(probe: &dyn Probe, addr: SocketAddr) -> bool {
    probe
        .get(addr, "/readyz")
        .is_ok_and(|reply| reply.status == 200)
}

/// The commit `/healthz` of the frontend at `addr` reports, if it answers 200 with one.
pub fn commit(probe: &dyn Probe, addr: SocketAddr) -> Option<String> {
    let reply = probe
        .get(addr, "/healthz")
        .ok()
        .filter(|r| r.status == 200)?;
    let json: serde_json::Value = serde_json::from_str(&reply.body).ok()?;
    json.get("commit")?.as_str().map(str::to_owned)
}
