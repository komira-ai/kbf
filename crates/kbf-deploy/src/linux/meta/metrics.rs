//! The write tier's state as kbf-server's `/metrics` reports it (the Prometheus text
//! format): what `meta-backup` reads before it stops the server.

use std::io::{self, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use kbf_host::step::HttpUrl;

use super::{Get, MetaError};

/// Acknowledged bytes on the write tier not yet in the object store.
pub const UNFLUSHED_BYTES: &str = "kbf_write_tier_unflushed_bytes";
/// Blocks holding such bytes.
pub const UNFLUSHED_BLOCKS: &str = "kbf_write_tier_unflushed_blocks";
/// Staged objects whose key in the store held another object.
pub const FLUSH_CONFLICTS: &str = "kbf_write_tier_flush_conflicts_total";

/// How long a `GET` of `/metrics` may take, connect and answer each.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The write tier's three readings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tier {
    pub bytes: u64,
    pub blocks: u64,
    pub conflicts: u64,
}

/// The write tier's readings at `url`, or `None` for a server with no write tier (none
/// of the three families). Refuses an answer that is not a 200, a family given twice or
/// with a value that is not a whole number, and a tier that reports some families but
/// not all.
pub fn read(get: Get<'_>, url: &HttpUrl) -> Result<Option<Tier>, MetaError> {
    let refuse = |why: String| MetaError::Metrics {
        url: url.to_string(),
        why,
    };
    let (status, body) = get(url).map_err(|e| refuse(e.to_string()))?;
    if status != 200 {
        return Err(refuse(format!("status {status}")));
    }
    let text = String::from_utf8(body).map_err(|_| refuse("the body is not UTF-8".into()))?;
    let value = |name: &'static str| family(&text, name, url);
    match (
        value(UNFLUSHED_BYTES)?,
        value(UNFLUSHED_BLOCKS)?,
        value(FLUSH_CONFLICTS)?,
    ) {
        (None, None, None) => Ok(None),
        (Some(bytes), Some(blocks), Some(conflicts)) => Ok(Some(Tier {
            bytes,
            blocks,
            conflicts,
        })),
        (b, k, _) => {
            let name = match (b, k) {
                (None, _) => UNFLUSHED_BYTES,
                (_, None) => UNFLUSHED_BLOCKS,
                _ => FLUSH_CONFLICTS,
            };
            Err(MetaError::Family {
                url: url.to_string(),
                name,
                why: "is missing beside the write tier's other families",
            })
        }
    }
}

/// The value of the sample `name` (a family without labels), if the text has one.
fn family(text: &str, name: &'static str, url: &HttpUrl) -> Result<Option<u64>, MetaError> {
    let bad = |why| MetaError::Family {
        url: url.to_string(),
        name,
        why,
    };
    let mut found = None;
    for l in text.lines() {
        let Some(rest) = l.strip_prefix(name) else {
            continue;
        };
        let Some(value) = rest.strip_prefix(' ') else {
            continue;
        };
        if found.is_some() {
            return Err(bad("is given twice"));
        }
        // kbf-server writes these as whole numbers (`name 0`).
        let n: u64 = value
            .trim()
            .parse()
            .map_err(|_| bad("has a value that is not a whole number"))?;
        found = Some(n);
    }
    Ok(found)
}

/// One plain-HTTP `GET` of `url` on the host: the status and the body (at most 1 MiB).
pub fn http_get(url: &HttpUrl) -> io::Result<(u16, Vec<u8>)> {
    let addr = (url.host(), url.port())
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::from(io::ErrorKind::AddrNotAvailable))?;
    let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    tcp.set_read_timeout(Some(TIMEOUT))?;
    tcp.set_write_timeout(Some(TIMEOUT))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: kbf-deploy\r\nConnection: close\r\n\r\n",
        url.path(),
        url.authority()
    );
    tcp.write_all(request.as_bytes())?;
    crate::https::read_reply(url, BufReader::new(tcp)).map_err(io::Error::other)
}
