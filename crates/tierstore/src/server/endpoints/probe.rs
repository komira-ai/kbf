//! The `/readyz` probe of an endpoint, and the address an endpoint URL names.

use std::io;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// Asks whether an endpoint is ready to serve. The cache's poller asks it of every endpoint
/// each interval; the tests answer it by hand.
#[async_trait]
pub(in crate::server) trait Probe: Send + Sync {
    /// Whether the endpoint (the URL as the file gives it) answered `GET /readyz` with 200 just
    /// now. Anything else, an answer that is late or not HTTP included, is `false`.
    async fn ready(&self, endpoint: &str) -> bool;
}

/// The `host:port` that the URL `http://host[:port]` names, with port 80 where the URL gives
/// none. `None` for a URL that has another scheme (`https://` too: the S3 client has no TLS), a path
/// (a trailing `/` too), a query, user information, an empty host, a port that is not
/// 1 to 65535, or a character that is not in a host name or an address (so the text is also safe
/// to show as a label of a metric).
pub(in crate::server) fn authority(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let allowed = |c: char| c.is_ascii_alphanumeric() || ".-_:[]".contains(c);
    if rest.is_empty() || !rest.chars().all(allowed) {
        return None;
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse::<u16>().ok()?),
        _ => (rest, 80),
    };
    let bracketed = host.starts_with('[') && host.ends_with(']');
    if host.is_empty() || port == 0 || (host.contains(':') && !bracketed) {
        return None;
    }
    Some(format!("{host}:{port}"))
}

/// The status in the first twelve bytes of an HTTP/1 answer (`HTTP/1.1 200`).
fn status_of(head: &[u8; 12]) -> Option<u16> {
    let (version, code) = head.split_at(9);
    if version != b"HTTP/1.1 " && version != b"HTTP/1.0 " {
        return None;
    }
    std::str::from_utf8(code).ok()?.parse().ok()
}

/// Sends `GET /readyz` to `authority` and gives the status of the answer.
async fn ask(authority: &str) -> io::Result<u16> {
    let mut stream = TcpStream::connect(authority).await?;
    let request = format!("GET /readyz HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    // A request that could not be sent shows as an answer that does not come.
    let _ = stream.write_all(request.as_bytes()).await;
    let mut head = [0; 12];
    stream.read_exact(&mut head).await?;
    status_of(&head).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an HTTP answer"))
}

/// A probe over a new connection to the endpoint each time, in plain HTTP (the S3 client of the
/// endpoints is plain HTTP too).
pub(in crate::server) struct HttpProbe {
    /// How long the connect, the request and the head of the answer may take together.
    timeout: Duration,
}

impl HttpProbe {
    /// A probe that gives up on an endpoint after `timeout`.
    pub(in crate::server) fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl Probe for HttpProbe {
    async fn ready(&self, endpoint: &str) -> bool {
        let Some(authority) = authority(endpoint) else {
            return false;
        };
        let asked = tokio::time::timeout(self.timeout, ask(&authority)).await;
        matches!(asked, Ok(Ok(200)))
    }
}
