//! The updater's end of the heartbeat: one `POST /v1/heartbeat` over mutual TLS, per
//! connection (`docs/design/updates.md`, sections 2 and 5).
//!
//! - **Who it talks to.** The server's certificate must chain to the farm CA file (`--ca`) for
//!   the coordinator's name (`--coordinator`), and must have the role `coordinator`
//!   (`urn:kbf:role:coordinator`): a certificate of any other role of the farm's, a storage
//!   node's say, is refused after the handshake and before the heartbeat is written.
//! - **Who it is.** The client certificate and key are read from their files for every
//!   heartbeat, so a renewed pair is used from the next one with no restart. The key file is
//!   held to the rule of the token file (a regular file of the updater's user, mode 0600 or
//!   0400). Nothing of the host's identity is in the body: the coordinator reads it from the
//!   certificate, and a certificate that names another node or role than the one the agent
//!   started as is not used (the drain it does targets the node it started as).
//! - **What it takes back.** A 200 whose body is exactly a [`Reply`] (a [`ServiceReply`] for
//!   an agent that lists services, an [`Answer`] for one that announces `agent_features`), read
//!   strictly and no more than [`REPLY_MAX`] bytes.
//!   Anything else is an error, and the updater does nothing with it but heartbeat again.
//!
//! HTTP/1.1 only, `Connection: close`, rustls with the ring provider over a std socket, like
//! the other clients of this crate: no async runtime.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_peer::uri_values_of;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::identity::{Identity, identity_of};
use crate::secret::read_secret;
use crate::wire::{Answer, HEARTBEAT_PATH, Heartbeat, Reply, ServiceReply};

/// The most bytes of a reply that are read.
pub const REPLY_MAX: usize = 16 * 1024;

/// The most bytes of the client key file that are read.
const KEY_MAX: u64 = 16 * 1024;

/// The role a coordinator's certificate names.
pub const COORDINATOR_ROLE: &str = "coordinator";

/// How long a connect, a write or a read may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Why a heartbeat did not get a reply.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoordinatorError {
    /// The coordinator could not be reached, or stopped answering.
    #[error("the coordinator is unreachable: {0}")]
    Unreachable(String),
    /// What answered is not the coordinator: its certificate does not chain to the farm CA,
    /// is not for its name, or is not a coordinator's.
    #[error("what answered is not the coordinator: {0}")]
    Untrusted(String),
    /// This host's own TLS files cannot be used.
    #[error("this host's TLS files cannot be used: {0}")]
    Files(String),
    /// The coordinator answered with a status other than 200.
    #[error("the coordinator answered HTTP {0}")]
    Status(u16),
    /// Its answer is not a reply.
    #[error("the coordinator's answer is not a reply: {0}")]
    Malformed(String),
}

/// The coordinator, as the agent uses it.
pub trait Coordinator: Send + Sync {
    /// Send `beat` and read the reply.
    ///
    /// # Errors
    /// [`CoordinatorError`].
    fn heartbeat(&self, beat: &Heartbeat) -> Result<Reply, CoordinatorError>;

    /// [`Coordinator::heartbeat`] for a heartbeat that lists `services`: the reply is read as a
    /// [`ServiceReply`], which may name a service. Only an agent that lists services calls it.
    ///
    /// # Errors
    /// [`CoordinatorError`].
    fn heartbeat_services(&self, beat: &Heartbeat) -> Result<ServiceReply, CoordinatorError>;

    /// [`Coordinator::heartbeat_services`] for a heartbeat that announces `agent_features`:
    /// the answer is the service reply and, from a coordinator that knows the field, the
    /// `features` it supports (an old one sends none). Only an agent that announces calls it.
    ///
    /// # Errors
    /// [`CoordinatorError`].
    fn heartbeat_answer(&self, beat: &Heartbeat) -> Result<Answer, CoordinatorError>;
}

/// The files of this host's side of the TLS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsFiles {
    /// The updater's certificate chain, leaf first (PEM).
    pub cert: PathBuf,
    /// Its private key (PEM).
    pub key: PathBuf,
    /// The farm CA the coordinator's certificate must chain to (PEM).
    pub ca: PathBuf,
}

/// Where the coordinator is: the name its certificate is for, and the port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The name, lower case.
    pub host: String,
    pub port: u16,
    name: ServerName<'static>,
}

impl Endpoint {
    /// `host:port`, where the host is a DNS name (not an address, not a bracketed one) and the
    /// port a number from 1 to 65535.
    ///
    /// # Errors
    /// What is wrong with `text`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (host, port) = text
            .rsplit_once(':')
            .ok_or("it must be <dns name>:<port>")?;
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or("its port must be a number from 1 to 65535")?;
        let host = host.to_ascii_lowercase();
        // What rustls takes for a DNS name: labels of letters, digits and hyphens, and a name
        // that is not an address.
        let name = ServerName::try_from(host.clone())
            .ok()
            .filter(|name| matches!(name, ServerName::DnsName(_)))
            .ok_or("its host must be a DNS name, not an address")?;
        Ok(Self { host, port, name })
    }
}

/// The real coordinator, over mutual TLS.
#[derive(Clone, Debug)]
pub struct HttpsCoordinator {
    endpoint: Endpoint,
    files: TlsFiles,
    timeout: Duration,
    identity: Option<Identity>,
}

impl HttpsCoordinator {
    pub fn new(endpoint: Endpoint, files: TlsFiles) -> Self {
        Self {
            endpoint,
            files,
            timeout: TIMEOUT,
            identity: None,
        }
    }

    /// Heartbeat only with a certificate that names `identity`: a pair swapped on disk for
    /// another node's is refused until the agent is restarted.
    #[must_use]
    pub fn bound_to(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// A failure of the connection, told as the coordinator being unreachable.
pub(crate) fn down(e: impl std::fmt::Display) -> CoordinatorError {
    CoordinatorError::Unreachable(e.to_string())
}

fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<TcpStream, CoordinatorError> {
    let addrs = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(down)?;
    let mut last = io::Error::other("the name has no address");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream.set_read_timeout(Some(timeout)).map_err(down)?;
                stream.set_write_timeout(Some(timeout)).map_err(down)?;
                return Ok(stream);
            }
            Err(e) => last = e,
        }
    }
    Err(down(last))
}

/// A connection to `endpoint`, TLS up, with this host's certificate presented and the server's
/// checked: it chains to the farm CA, is for the endpoint's name, and names the role
/// `server_role` (and no other). The files are read now, so a renewed pair is used from the
/// next call; with `bound` the certificate must still name that identity.
///
/// # Errors
/// [`CoordinatorError`]: the files, the connection, or what answered.
pub(crate) fn dial(
    endpoint: &Endpoint,
    files: &TlsFiles,
    bound: Option<&Identity>,
    timeout: Duration,
    server_role: &str,
) -> Result<StreamOwned<ClientConnection, TcpStream>, CoordinatorError> {
    let (config, leaf) = load(files)?;
    if let Some(expected) = bound {
        let now = identity_of(leaf.as_ref())
            .map_err(|e| CoordinatorError::Files(format!("{}: {e}", files.cert.display())))?;
        if &now != expected {
            return Err(CoordinatorError::Files(format!(
                "{} now names another node or role than the agent started as; restart it",
                files.cert.display()
            )));
        }
    }
    let connection = ClientConnection::new(config, endpoint.name.clone()).map_err(down)?;
    let mut tls = StreamOwned::new(connection, connect(endpoint, timeout)?);
    // The handshake first: what answered must be the right server before anything is said.
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|e| handshake_error(&e))?;
    }
    check_role(&tls.conn, server_role)?;
    Ok(tls)
}

/// The TLS client configuration the files make.
///
/// # Errors
/// [`CoordinatorError::Files`] for a file that is not there or not PEM of its kind, a key that
/// is not the certificate's, or a CA file with no usable certificate.
pub fn client_config(files: &TlsFiles) -> Result<Arc<ClientConfig>, CoordinatorError> {
    load(files).map(|(config, _)| config)
}

/// [`client_config`], and the DER of the leaf certificate it was made from.
fn load(
    files: &TlsFiles,
) -> Result<(Arc<ClientConfig>, CertificateDer<'static>), CoordinatorError> {
    let bad = |path: &Path, why: &dyn std::fmt::Display| {
        CoordinatorError::Files(format!("{}: {why}", path.display()))
    };
    let read = |path: &Path| std::fs::read(path).map_err(|e| bad(path, &e));
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&read(&files.cert)?)
        .collect::<Result<_, _>>()
        .map_err(|e| bad(&files.cert, &e))?;
    let Some(leaf) = chain.first().cloned() else {
        return Err(bad(&files.cert, &"it holds no certificate"));
    };
    let key_pem =
        read_secret(&files.key, "--client-key", KEY_MAX).map_err(|e| bad(&files.key, &e))?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| bad(&files.key, &e))?;
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(&read(&files.ca)?) {
        let ca = ca.map_err(|e| bad(&files.ca, &e))?;
        roots.add(ca).map_err(|e| bad(&files.ca, &e))?;
    }
    if roots.is_empty() {
        return Err(bad(&files.ca, &"it holds no certificate"));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder =
        ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions();
    let mut config = builder
        .and_then(|b| {
            b.with_root_certificates(roots)
                .with_client_auth_cert(chain, key)
        })
        .map_err(|e| CoordinatorError::Files(format!("not a TLS configuration: {e}")))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((Arc::new(config), leaf))
}

/// Whether `error`, from the TLS stream, is the server's certificate being refused.
fn refused_certificate(error: &io::Error) -> Option<String> {
    let inner = error.get_ref()?.downcast_ref::<rustls::Error>()?;
    matches!(inner, rustls::Error::InvalidCertificate(_)).then(|| inner.to_string())
}

/// A failure of the handshake: the server's certificate refused is the server not being the
/// coordinator, anything else is the connection failing.
fn handshake_error(error: &io::Error) -> CoordinatorError {
    match refused_certificate(error) {
        Some(why) => CoordinatorError::Untrusted(why),
        None => CoordinatorError::Unreachable(error.to_string()),
    }
}

impl HttpsCoordinator {
    /// Post `beat` and read the reply as a `T`.
    fn post<T: serde::de::DeserializeOwned>(
        &self,
        beat: &Heartbeat,
    ) -> Result<T, CoordinatorError> {
        let mut tls = dial(
            &self.endpoint,
            &self.files,
            self.identity.as_ref(),
            self.timeout,
            COORDINATOR_ROLE,
        )?;
        // A heartbeat is strings and two enums: it serializes.
        let body = serde_json::to_vec(beat).unwrap_or_default();
        let head = format!(
            "POST {HEARTBEAT_PATH} HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.endpoint.host,
            self.endpoint.port,
            body.len()
        );
        tls.write_all(head.as_bytes()).map_err(down)?;
        tls.write_all(&body).map_err(down)?;
        tls.flush().map_err(down)?;
        parse_reply(&read_all(&mut tls, self.timeout, REPLY_MAX + 8 * 1024)?)
    }
}

impl Coordinator for HttpsCoordinator {
    fn heartbeat(&self, beat: &Heartbeat) -> Result<Reply, CoordinatorError> {
        self.post(beat)
    }

    fn heartbeat_services(&self, beat: &Heartbeat) -> Result<ServiceReply, CoordinatorError> {
        self.post(beat)
    }

    fn heartbeat_answer(&self, beat: &Heartbeat) -> Result<Answer, CoordinatorError> {
        self.post(beat)
    }
}

/// The server's certificate names the role `role`, and only that. (A certificate that cannot
/// be read names no role.)
fn check_role(conn: &ClientConnection, role: &str) -> Result<(), CoordinatorError> {
    let leaf = conn
        .peer_certificates()
        .and_then(<[_]>::first)
        .map_or(&[][..], AsRef::as_ref);
    let roles = uri_values_of(leaf, kbf_peer::ROLE_URI_PREFIX).unwrap_or_default();
    if roles == [role] {
        Ok(())
    } else {
        Err(CoordinatorError::Untrusted(format!(
            "its certificate's role is not `{role}` (it names {roles:?})"
        )))
    }
}

/// Everything the server sends, up to `limit` bytes (a body and a head). A server that closes
/// without the TLS close is read as ended: the length of the body decides whether it is whole.
pub(crate) fn read_all(
    tls: &mut StreamOwned<ClientConnection, TcpStream>,
    window: Duration,
    limit: usize,
) -> Result<Vec<u8>, CoordinatorError> {
    let end = Instant::now() + window;
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        // The whole reply has `window`, not each read: a server that trickles is cut off.
        let left = end.saturating_duration_since(Instant::now());
        let read = tls
            .sock
            .set_read_timeout(Some(left))
            .and_then(|()| tls.read(&mut chunk));
        match read {
            Ok(0) => return Ok(raw),
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > limit {
                    return Err(CoordinatorError::Malformed("it is too long".to_owned()));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(raw),
            Err(e) => return Err(CoordinatorError::Unreachable(e.to_string())),
        }
    }
}

/// The reply in the HTTP response `raw`.
fn parse_reply<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T, CoordinatorError> {
    let malformed = |why: &str| CoordinatorError::Malformed(why.to_owned());
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    let head = match response.parse(raw) {
        Ok(httparse::Status::Complete(head)) => head,
        Ok(httparse::Status::Partial) if raw.is_empty() => {
            return Err(CoordinatorError::Unreachable(
                "it closed the connection without an answer".to_owned(),
            ));
        }
        _ => return Err(malformed("not an HTTP response")),
    };
    let status = response.code.unwrap_or(0);
    if status != 200 {
        return Err(CoordinatorError::Status(status));
    }
    let header = |name: &str| {
        response
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value)
    };
    if header("transfer-encoding").is_some() {
        return Err(malformed("a body of unknown length"));
    }
    let length = header("content-length")
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| malformed("a body with no length"))?;
    let body = &raw[head..];
    if length > REPLY_MAX {
        return Err(malformed("it is too long"));
    }
    if body.len() != length {
        return Err(malformed("the body is not as long as it says"));
    }
    serde_json::from_slice(body).map_err(|e| CoordinatorError::Malformed(e.to_string()))
}
