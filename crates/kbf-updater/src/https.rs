//! A client of kbf-proxy's own routes: one `GET` over HTTP/1.1 and TLS, with the operator
//! bearer token. `GET /v1/proxy/swap` is served by the proxy on its public port (the update
//! class of its route table), not by the frontend child behind it, so the plain loopback
//! operator API of [`crate::operator`] does not reach it.
//!
//! The server is verified against the farm CA file given by `--proxy-ca-file` and nothing
//! else: this crate trusts no system root. The name checked is `--proxy-server-name`, or the
//! address's IP when there is none. The proxy asks for no client certificate on this class,
//! only the token. The token is the one of [`crate::secret`], read on every call.
//!
//! Error texts carry no address and no token: they reach `GET /rolls/{id}`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::operator::{REPLY_MAX, TIMEOUT};
use crate::ports::PortError;
use crate::reply::Reply;
use crate::secret::read_token;

/// The flag that names the CA file, which error texts name instead of its path.
pub const CA_FLAG: &str = "--proxy-ca-file";

/// What error texts call the far side.
pub const WHAT: &str = "kbf-proxy";

/// What an operator checks when the proxy refuses a call.
pub const CHECK: &str = "--token-file and the proxy's operator token";

/// Where the proxy is and how to trust it: the three `--proxy-*` flags.
#[derive(Clone, Debug)]
pub struct ProxyTls {
    /// The proxy's public address, a loopback one (the updater runs on the frontend host).
    pub addr: SocketAddr,
    /// PEM of the farm CA that vouches for the proxy's certificate.
    pub ca_file: PathBuf,
    /// The DNS name the certificate carries; the address's IP when there is none.
    pub server_name: Option<String>,
}

/// kbf-proxy's public port. It has no `Debug`: nothing about it should be printed.
pub struct ProxyApi {
    addr: SocketAddr,
    name: ServerName<'static>,
    config: Arc<ClientConfig>,
    token_file: PathBuf,
    timeout: Duration,
}

fn invalid(why: &str) -> std::io::Error {
    std::io::Error::other(format!("{CA_FLAG}: {why}"))
}

/// The certificates in the file at `path`, at least one.
fn read_roots(path: &Path) -> std::io::Result<RootCertStore> {
    let pem = std::fs::read(path).map_err(|e| invalid(&e.kind().to_string()))?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("it is not PEM"))?;
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .map_err(|_| invalid("it holds a certificate that cannot be a trust anchor"))?;
    }
    if roots.is_empty() {
        return Err(invalid("it holds no certificate"));
    }
    Ok(roots)
}

impl ProxyApi {
    /// A client for the proxy at `tls.addr`. The CA file and the token file are read once now,
    /// so that a start-up with a bad file fails at once and not at the first roll; the token
    /// file is read again on each call. The errors name the flag, never a path.
    pub fn new(tls: &ProxyTls, token_file: PathBuf) -> std::io::Result<ProxyApi> {
        read_token(&token_file).map_err(std::io::Error::other)?;
        let name = match &tls.server_name {
            Some(name) => ServerName::try_from(name.clone()).map_err(|_| {
                std::io::Error::other("--proxy-server-name must be a DNS name or an IP")
            })?,
            None => ServerName::IpAddress(tls.addr.ip().to_canonical().into()),
        };
        let roots = read_roots(&tls.ca_file)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(std::io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(ProxyApi {
            addr: tls.addr,
            name,
            config: Arc::new(config),
            token_file,
            timeout: TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> ProxyApi {
        self.timeout = timeout;
        self
    }

    /// `GET path`. A refused or timed-out connection and a cut answer are
    /// [`PortError::unreachable`]; a certificate the CA file does not vouch for is an answer
    /// that says no ([`PortError::other`]); the status is the caller's to judge.
    pub fn get(&self, path: &str) -> Result<Reply, PortError> {
        let token = read_token(&self.token_file).map_err(PortError::other)?;
        let raw = self.exchange(path, &token).map_err(|e| match e.kind() {
            // rustls reports a refused handshake (a certificate, a version) as invalid data.
            std::io::ErrorKind::InvalidData => PortError::other(format!(
                "{WHAT}'s TLS was refused; check {CA_FLAG} and --proxy-server-name"
            )),
            kind => PortError::unreachable(format!("{WHAT}: {kind}")),
        })?;
        Reply::parse(&raw, WHAT)
    }

    fn exchange(&self, path: &str, token: &str) -> std::io::Result<Vec<u8>> {
        let tcp = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        tcp.set_read_timeout(Some(self.timeout))?;
        tcp.set_write_timeout(Some(self.timeout))?;
        let conn = ClientConnection::new(Arc::clone(&self.config), self.name.clone())
            .map_err(std::io::Error::other)?;
        let mut tls = StreamOwned::new(conn, tcp);
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Accept: application/json\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: 0\r\n\r\n",
            self.addr
        );
        tls.write_all(request.as_bytes())?;
        tls.flush()?;
        let mut raw = Vec::new();
        // A peer that closes the TCP connection without the TLS goodbye ends the answer; its
        // length is checked against `Content-Length` by `Reply::parse`.
        match (&mut tls).take(REPLY_MAX).read_to_end(&mut raw) {
            Err(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => return Err(e),
            _ => {}
        }
        tls.conn.send_close_notify();
        // The answer is in hand: whether the goodbye is delivered does not matter.
        let _ = tls.flush();
        Ok(raw)
    }
}
