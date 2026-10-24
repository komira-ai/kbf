//! A client of the frontend's loopback operator API (`docs/api.md`): one `GET` or `POST` with
//! the operator bearer token from a file, and the proxy's per-boot secret when the frontend runs
//! behind kbf-proxy. Safe-to-stop ([`crate::safe_http`]) and node health
//! ([`crate::node_health`]) are reads of it; draining a node ([`crate::node_drain`]) is a read
//! and two writes. The same client, pointed at the operator port of a pool member
//! ([`OperatorApi::at`]), retires that frontend ([`crate::frontend_http`]); and without
//! credentials ([`OperatorApi::anonymous`]) it speaks to the proxy's admin API
//! ([`crate::pool_http`]).
//!
//! A frontend that runs behind kbf-proxy (`--proxy-auth-file`) refuses an operator call that
//! lacks the proxy's per-boot secret, 403 (`kbf-server` `api.rs` `operator_gate`). With
//! `--operator-proxy-auth-file` the client reads that secret on every call (it changes at
//! each proxy boot) and sends it as `x-kbf-proxy-auth`, 64 hex digits. Both files are held
//! to the rule of [`crate::secret`].
//!
//! A host's updater has no token and does not reach the loopback API: [`OperatorApi::as_updater`]
//! calls the same API through the frontend's public proxy over mutual TLS, presenting the
//! updater's certificate, which the proxy admits for the node in that certificate and no
//! other (`docs/design/updates.md`, "Trust"). It sends no `Authorization` header and no proxy
//! secret: the proxy puts the token in itself, so the host never holds it. The files are
//! read on every call, so a renewed certificate is used at once.
//!
//! The request has the headers of `kbf cluster`, including a JSON content type: the gated
//! routes want one on every call, a `GET` too. A `POST` carries a JSON body or none.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::agent::coordinator::{CoordinatorError, Endpoint, TlsFiles, dial, down, read_all};
use crate::identity::Identity;
use crate::ports::PortError;
use crate::reply::Reply;
use crate::secret::{read_proxy_secret, read_token};

/// How long a connect, a write or a read may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes of an answer that are read.
pub(crate) const REPLY_MAX: u64 = 1024 * 1024;

/// What error texts call the far side.
pub const WHAT: &str = "the operator API";

/// What the error texts call the proxy's admin API.
pub const PROXY_WHAT: &str = "the proxy's admin API";

/// What an operator checks when the API refuses a call: the flags of the two files (named in
/// [`crate::secret`]) and the API's own gates.
pub const CHECK: &str = "--token-file, --operator-proxy-auth-file and the API's gates";

/// The header the proxy's secret travels in, as 64 lower-case hex digits.
const PROXY_AUTH_HEADER: &str = "x-kbf-proxy-auth";

/// The role the proxy's certificate names.
const FRONTEND_ROLE: &str = "frontend";

/// Where a call goes.
#[derive(Clone)]
enum Wire {
    /// A loopback address, in the clear.
    Loopback(SocketAddr),
    /// The frontend's public proxy, over mutual TLS with a host updater's certificate.
    Proxy(Arc<Proxy>),
}

/// The frontend's proxy and the files a host's updater presents to it.
struct Proxy {
    endpoint: Endpoint,
    files: TlsFiles,
    identity: Identity,
}

/// The operator API at one address. It has no `Debug`: nothing about it should be printed.
#[derive(Clone)]
pub struct OperatorApi {
    wire: Wire,
    /// What error texts call the far side.
    what: &'static str,
    /// The file of the operator token; none for a call that sends no credential.
    token_file: Option<PathBuf>,
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
            wire: Wire::Loopback(addr),
            what: WHAT,
            token_file: Some(token_file),
            proxy_auth_file,
            timeout: TIMEOUT,
        })
    }

    /// A client for the proxy's admin API, which asks for no credential and listens on loopback
    /// only: it sends neither the token nor the proxy's secret, so neither leaves this
    /// process for a socket that does not need it.
    pub fn anonymous(addr: SocketAddr) -> OperatorApi {
        OperatorApi {
            wire: Wire::Loopback(addr),
            what: PROXY_WHAT,
            token_file: None,
            proxy_auth_file: None,
            timeout: TIMEOUT,
        }
    }

    /// A client for the operator API through the frontend's public proxy at `endpoint`, for
    /// the host whose updater certificate (in `files`) names `identity`: mutual TLS, no token.
    /// The proxy admits the calls about that node and refuses every other; a certificate that
    /// now names another node than `identity` is not presented.
    pub fn as_updater(endpoint: Endpoint, files: TlsFiles, identity: Identity) -> OperatorApi {
        OperatorApi {
            wire: Wire::Proxy(Arc::new(Proxy {
                endpoint,
                files,
                identity,
            })),
            what: WHAT,
            token_file: None,
            proxy_auth_file: None,
            timeout: TIMEOUT,
        }
    }

    /// What this client's error texts call the API it talks to.
    pub fn what(&self) -> &'static str {
        self.what
    }

    /// The same client, with the same files, for the API at another address (another member
    /// of a pool has its own operator port).
    pub fn at(&self, addr: SocketAddr) -> OperatorApi {
        OperatorApi {
            wire: Wire::Loopback(addr),
            ..self.clone()
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> OperatorApi {
        self.timeout = timeout;
        self
    }

    /// `GET path`. A refused or timed-out connection and a cut answer are
    /// [`PortError::unreachable`]; the status is the caller's to judge.
    pub fn get(&self, path: &str) -> Result<Reply, PortError> {
        self.call("GET", path, "")
    }

    /// `POST path` with `body` (JSON text, or empty for none). Failures as for
    /// [`OperatorApi::get`]; the status is the caller's to judge.
    pub fn post(&self, path: &str, body: &str) -> Result<Reply, PortError> {
        self.call("POST", path, body)
    }

    fn call(&self, method: &str, path: &str, body: &str) -> Result<Reply, PortError> {
        let token = match &self.token_file {
            Some(file) => Some(read_token(file).map_err(PortError::other)?),
            None => None,
        };
        let secret = match &self.proxy_auth_file {
            Some(file) => Some(read_proxy_secret(file).map_err(PortError::other)?),
            None => None,
        };
        let what = self.what;
        let raw = self.exchange(method, path, body, token.as_deref(), secret.as_deref())?;
        Reply::parse(&raw, what)
    }

    /// The request, with the headers of `kbf cluster`.
    fn request(
        host: &str,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
        proxy_secret: Option<&str>,
    ) -> String {
        let bearer = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let proxy = proxy_secret
            .map(|secret| format!("{PROXY_AUTH_HEADER}: {secret}\r\n"))
            .unwrap_or_default();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
             Accept: application/json\r\n{bearer}\
             {proxy}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// One request, one answer: the raw bytes, up to [`REPLY_MAX`].
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: &str,
        token: Option<&str>,
        proxy_secret: Option<&str>,
    ) -> Result<Vec<u8>, PortError> {
        let what = self.what;
        match &self.wire {
            Wire::Loopback(addr) => {
                let request =
                    Self::request(&addr.to_string(), method, path, body, token, proxy_secret);
                self.loopback(*addr, request.as_bytes())
                    .map_err(|e| PortError::unreachable(format!("{what}: {}", e.kind())))
            }
            Wire::Proxy(proxy) => {
                let host = format!("{}:{}", proxy.endpoint.host, proxy.endpoint.port);
                let request = Self::request(&host, method, path, body, None, None);
                self.through(proxy, request.as_bytes())
                    .map_err(|e| match e {
                        CoordinatorError::Files(_) => PortError::other(format!(
                            "{what}: this host's TLS files cannot be used"
                        )),
                        CoordinatorError::Untrusted(_) => PortError::other(format!(
                            "{what}: what answered is not the frontend (its certificate is not \
                         the farm's, not for its name, or not a frontend's)"
                        )),
                        _ => PortError::unreachable(format!("{what}: unreachable")),
                    })
            }
        }
    }

    fn loopback(&self, addr: SocketAddr, request: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        stream.write_all(request)?;
        let mut raw = Vec::new();
        stream.take(REPLY_MAX).read_to_end(&mut raw)?;
        Ok(raw)
    }

    fn through(&self, proxy: &Proxy, request: &[u8]) -> Result<Vec<u8>, CoordinatorError> {
        let mut tls = dial(
            &proxy.endpoint,
            &proxy.files,
            Some(&proxy.identity),
            self.timeout,
            FRONTEND_ROLE,
        )?;
        tls.write_all(request).map_err(down)?;
        tls.flush().map_err(down)?;
        read_all(
            &mut tls,
            self.timeout,
            usize::try_from(REPLY_MAX).unwrap_or(usize::MAX),
        )
    }
}
