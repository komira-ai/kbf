//! A client of the frontend's loopback operator API (`docs/api.md`): one `GET` with the
//! operator bearer token from a file, and the proxy's per-boot secret when the frontend runs
//! behind kbf-proxy. Safe-to-stop ([`crate::safe_http`]) and node health
//! ([`crate::node_health`]) are both calls of it.
//!
//! A frontend that runs behind kbf-proxy (`--proxy-auth-file`) refuses an operator call that
//! lacks the proxy's per-boot secret, 403 (`kbf-server` `api.rs` `operator_gate`). With
//! `--operator-proxy-auth-file` the client reads that secret on every call (it changes at
//! each proxy boot) and sends it as `x-kbf-proxy-auth`, 64 hex digits. Both files are held
//! to the rule of [`crate::secret`].
//!
//! The request has the headers of `kbf cluster`, including a JSON content type: the gated
//! routes want one on every call, a `GET` too.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use crate::ports::PortError;
use crate::reply::Reply;
use crate::secret::{read_proxy_secret, read_token};

/// How long a connect, a write or a read may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes of an answer that are read.
pub(crate) const REPLY_MAX: u64 = 1024 * 1024;

/// What error texts call the far side.
pub const WHAT: &str = "the operator API";

/// What an operator checks when the API refuses a call: the flags of the two files (named in
/// [`crate::secret`]) and the API's own gates.
pub const CHECK: &str = "--token-file, --operator-proxy-auth-file and the API's gates";

/// The header the proxy's secret travels in, as 64 lower-case hex digits.
const PROXY_AUTH_HEADER: &str = "x-kbf-proxy-auth";

/// The operator API at one address. It has no `Debug`: nothing about it should be printed.
pub struct OperatorApi {
    addr: SocketAddr,
    token_file: PathBuf,
    /// The proxy's secret, for a frontend behind kbf-proxy.
    proxy_auth_file: Option<PathBuf>,
    timeout: Duration,
}

impl OperatorApi {
    /// A client for the API at `addr`. The token file, and the proxy's secret file if there
    /// is one, are read once now, so that a start-up with a bad file fails at once and not at
    /// the first roll; each call reads them again.
    pub fn new(
        addr: SocketAddr,
        token_file: PathBuf,
        proxy_auth_file: Option<PathBuf>,
    ) -> Result<OperatorApi, String> {
        read_token(&token_file)?;
        if let Some(path) = &proxy_auth_file {
            read_proxy_secret(path)?;
        }
        Ok(OperatorApi {
            addr,
            token_file,
            proxy_auth_file,
            timeout: TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> OperatorApi {
        self.timeout = timeout;
        self
    }

    /// `GET path`. A refused or timed-out connection and a cut answer are
    /// [`PortError::unreachable`]; the status is the caller's to judge.
    pub fn get(&self, path: &str) -> Result<Reply, PortError> {
        let token = read_token(&self.token_file).map_err(PortError::other)?;
        let secret = match &self.proxy_auth_file {
            Some(file) => Some(read_proxy_secret(file).map_err(PortError::other)?),
            None => None,
        };
        let raw = self
            .exchange(path, &token, secret.as_deref())
            .map_err(|e| PortError::unreachable(format!("{WHAT}: {}", e.kind())))?;
        Reply::parse(&raw, WHAT)
    }

    /// One request, one answer: the raw bytes, up to [`REPLY_MAX`].
    fn exchange(
        &self,
        path: &str,
        token: &str,
        proxy_secret: Option<&str>,
    ) -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let proxy = proxy_secret
            .map(|secret| format!("{PROXY_AUTH_HEADER}: {secret}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Accept: application/json\r\nAuthorization: Bearer {token}\r\n\
             {proxy}Content-Type: application/json\r\nContent-Length: 0\r\n\r\n",
            self.addr
        );
        stream.write_all(request.as_bytes())?;
        let mut raw = Vec::new();
        stream.take(REPLY_MAX).read_to_end(&mut raw)?;
        Ok(raw)
    }
}
