//! The question a member is asked to learn whether it is ready: `GET /readyz` on its operator
//! listener (`crate::pool_probe` asks it of every member, once a second).
//!
//! The proxy has one writer for each member's readiness, its own poll (design note decision
//! C10). The poll asks the member's operator listener over plain HTTP/1.1 on loopback, and
//! gives it 3 s to answer. A failed poll does not close a stream or refuse a new one: a member
//! that answers late is still the member its streams are on.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use crate::pool::Connector;

/// How often a member is polled.
pub(crate) const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long a member has to answer one poll, connecting included.
pub(crate) const POLL_TIMEOUT: Duration = Duration::from_secs(3);

/// The request of a poll: the member's own readiness path, HTTP/1.1, closed after the answer.
const REQUEST: &[u8] = b"GET /readyz HTTP/1.1\r\nHost: kbf-proxy\r\nConnection: close\r\n\r\n";
/// The start of a 200 status line; the poll reads no more of the answer than this.
pub(crate) const OK_LINE: &[u8] = b"HTTP/1.1 200";

/// The connector the polls dial with.
pub(crate) struct Readiness {
    connector: Connector,
}

impl Readiness {
    pub(crate) fn new(connector: Connector) -> Self {
        Self { connector }
    }

    /// One poll of `addr`: true only for a 200 within [`POLL_TIMEOUT`]. A refused connection,
    /// a hang, a reset, a 503 and anything that is not HTTP are all "not ready".
    pub(crate) async fn ask(&self, addr: SocketAddr) -> bool {
        let asked = async {
            let mut io = (self.connector)(addr).await.ok()?;
            io.write_all(REQUEST).await.ok()?;
            let mut line = [0_u8; OK_LINE.len()];
            io.read_exact(&mut line).await.ok()?;
            Some(line == *OK_LINE)
        };
        timeout(POLL_TIMEOUT, asked)
            .await
            .ok()
            .flatten()
            .unwrap_or(false)
    }
}
