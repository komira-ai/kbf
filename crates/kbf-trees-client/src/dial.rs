//! How the client reaches a node: TCP to its host and port, then, with a pinned CA, TLS 1.2
//! or 1.3 with the `ring` provider, trusting that CA and no system root, presenting no client
//! certificate and offering `h2`. tonic's channel calls this for every connection it makes,
//! under the connect timeout; tonic marks a failure here as a connection that was never made,
//! which is how [`Error::Unreachable`](crate::Error::Unreachable) knows nothing was sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::codegen::Service;

use crate::config::Node;
use crate::error::ConfigError;

/// A connection, over TLS or not.
pub(crate) trait Io: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// Why a connection could not be made.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DialError {
    /// The TCP connect failed.
    #[error("connecting: {0}")]
    Connect(#[source] std::io::Error),
    /// The TLS handshake failed: the node's certificate is not the pinned CA's, or does not
    /// carry the node's host.
    #[error("the TLS handshake: {0}")]
    Handshake(#[source] std::io::Error),
    /// Connecting took longer than the connect timeout.
    #[error("no connection within {0:?}")]
    TimedOut(Duration),
}

/// The connector of one node.
#[derive(Clone)]
pub(crate) struct Dial {
    host: String,
    port: u16,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    timeout: Duration,
}

impl Dial {
    /// The connector of `node`, over TLS trusting `ca_pem` alone when one is given.
    pub(crate) fn new(
        node: &Node,
        ca_pem: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Self, ConfigError> {
        let tls = ca_pem.map(|pem| connector(node, pem)).transpose()?;
        Ok(Self {
            host: node.host().to_owned(),
            port: node.port(),
            tls,
            timeout,
        })
    }

    async fn dial(self) -> Result<TokioIo<Box<dyn Io>>, DialError> {
        let target = (self.host.as_str(), self.port);
        let tcp = TcpStream::connect(target)
            .await
            .map_err(DialError::Connect)?;
        // Calls are answered at once; Nagle's delay would only add latency. A socket that
        // refuses the option still works, so the result is not needed.
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = match self.tls {
            Some((connector, name)) => Box::new(
                connector
                    .connect(name, tcp)
                    .await
                    .map_err(DialError::Handshake)?,
            ),
            None => Box::new(tcp),
        };
        Ok(TokioIo::new(io))
    }
}

impl Service<Uri> for Dial {
    type Response = TokioIo<Box<dyn Io>>;
    type Error = DialError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, DialError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), DialError>> {
        Poll::Ready(Ok(()))
    }

    /// Dials the node; the URI tonic passes is ignored, since a client has one node.
    fn call(&mut self, _: Uri) -> Self::Future {
        let dial = self.clone();
        let limit = self.timeout;
        Box::pin(async move {
            tokio::time::timeout(limit, dial.dial())
                .await
                .unwrap_or(Err(DialError::TimedOut(limit)))
        })
    }
}

/// The TLS connector that trusts `pem` and nothing else, and the name the node's certificate
/// must carry: its host, a DNS name or an IP address.
fn connector(node: &Node, pem: &[u8]) -> Result<(TlsConnector, ServerName<'static>), ConfigError> {
    let name = ServerName::try_from(node.host().to_owned()).map_err(|_| ConfigError::Address {
        address: node.authority().to_owned(),
        reason: "not a name a certificate can carry",
    })?;
    let config = client_config(roots(pem)?)?;
    Ok((TlsConnector::from(Arc::new(config)), name))
}

/// The trust anchors: every certificate of `pem`, and nothing else (no system root).
fn roots(pem: &[u8]) -> Result<rustls::RootCertStore, ConfigError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem) {
        let cert = cert.map_err(|cause| ConfigError::BadCa(cause.to_string()))?;
        roots
            .add(cert)
            .map_err(|cause| ConfigError::BadCa(cause.to_string()))?;
    }
    if roots.is_empty() {
        return Err(ConfigError::BadCa("no certificate".to_owned()));
    }
    Ok(roots)
}

/// The TLS settings of a client: the `ring` provider, its safe default versions (TLS 1.2 and
/// 1.3), `roots` as the only trust anchors, no client certificate, and `h2` offered.
fn client_config(roots: rustls::RootCertStore) -> Result<rustls::ClientConfig, ConfigError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // gRPC runs over HTTP/2, which TLS names `h2`.
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(config)
}

#[cfg(test)]
#[path = "dial_tests.rs"]
mod tests;
