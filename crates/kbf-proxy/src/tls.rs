//! The public listener's TLS: the configuration that certificate, key and client CA make
//! together, reloaded as one unit, and the handshake that turns an accepted connection into
//! a verified peer.
//!
//! - **Reload.** [`TlsReloader::reload`] re-reads all three files and builds a new
//!   configuration; it replaces the old one only if the whole build succeeds, so a handshake
//!   never sees a new certificate with the old CA or the reverse. A failed reload keeps the
//!   old configuration, ticks `kbf_proxy_tls_reload_failed_total` and logs the error (which
//!   names a file, never its content). Only handshakes that start afterwards see the new
//!   configuration; an open connection keeps the one it was accepted under.
//! - **Handshake.** A client certificate is optional, and one that fails verification is a
//!   handshake error, never an anonymous client (see
//!   [`kbf_tls::server_config_optional_client`]). A certificate that verifies is read once,
//!   by [`identity_of`]: its role URI and its one DNS name. A certificate that verifies but
//!   names no usable identity (two names, a wildcard, no role) closes the connection: an
//!   identity that must be guessed is not one.
//! - **Protocol.** ALPN `h2` is HTTP/2; `http/1.1` or no ALPN is HTTP/1.1, which the proxy
//!   serves only for `/v1/*` and the probes (see [`crate::h1`]).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

use crate::identity::{Identity, identity_of};
use crate::metrics::Metrics;
use crate::pool::BoxIo;

/// The three files the configuration is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsPaths {
    /// The PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// The PEM private key of the leaf.
    pub key: PathBuf,
    /// The PEM bundle of the farm CA that client certificates must chain to.
    pub client_ca: PathBuf,
}

/// Why a TLS configuration could not be made.
#[derive(Debug, thiserror::Error)]
pub enum TlsReloadError {
    /// A file could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The reason.
        #[source]
        source: std::io::Error,
    },
    /// The files do not make a configuration.
    #[error(transparent)]
    Config(#[from] kbf_tls::TlsError),
}

fn read(path: &PathBuf) -> Result<Vec<u8>, TlsReloadError> {
    std::fs::read(path).map_err(|source| TlsReloadError::Read {
        path: path.clone(),
        source,
    })
}

/// The configuration the next handshake uses.
pub(crate) struct TlsReloader {
    paths: TlsPaths,
    current: RwLock<Arc<ServerConfig>>,
    metrics: Arc<Metrics>,
}

impl std::fmt::Debug for TlsReloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsReloader")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl TlsReloader {
    /// Reads the files and builds the first configuration. A server does not start on a bad
    /// file; an expired certificate is accepted here, as it always was, and refused by a
    /// reload.
    ///
    /// # Errors
    /// [`TlsReloadError`].
    pub(crate) fn load(paths: TlsPaths, metrics: Arc<Metrics>) -> Result<Self, TlsReloadError> {
        let config = Self::build(&paths, false)?;
        Ok(Self {
            paths,
            current: RwLock::new(config),
            metrics,
        })
    }

    fn build(paths: &TlsPaths, reject_expired: bool) -> Result<Arc<ServerConfig>, TlsReloadError> {
        let (cert, key, ca) = (
            read(&paths.cert)?,
            read(&paths.key)?,
            read(&paths.client_ca)?,
        );
        Ok(kbf_tls::server_config_optional_client(
            &cert,
            &key,
            &ca,
            reject_expired,
        )?)
    }

    /// The configuration a handshake that starts now uses.
    pub(crate) fn config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Re-reads certificate, key and client CA, and replaces the configuration if the three
    /// make a working one. On any failure the old configuration stays, the failure is
    /// counted and logged, and the error is returned.
    ///
    /// # Errors
    /// [`TlsReloadError`].
    pub(crate) fn reload(&self) -> Result<(), TlsReloadError> {
        match Self::build(&self.paths, true) {
            Ok(config) => {
                *self.current.write().unwrap_or_else(PoisonError::into_inner) = config;
                tracing::info!("reloaded the proxy's TLS files");
                Ok(())
            }
            Err(error) => {
                self.metrics
                    .tls_reload_failed
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%error, "TLS files unusable; the previous ones are kept");
                Err(error)
            }
        }
    }
}

/// Which HTTP version a connection speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Proto {
    /// HTTP/2 (ALPN `h2`; prior knowledge on the test listener).
    H2,
    /// HTTP/1.1 (ALPN `http/1.1`, or no ALPN).
    H1,
}

/// Who is on the other end of a connection, as the handshake left it. Every request of the
/// connection carries it in its extensions.
#[derive(Clone, Debug)]
pub(crate) struct Peer {
    /// The verified certificate's identity; `None` for a client that presented none.
    pub(crate) identity: Option<Identity>,
    /// The client's address.
    pub(crate) ip: IpAddr,
    /// Whether the request came over HTTP/1.1.
    pub(crate) h1: bool,
}

/// A connection after its handshake.
pub(crate) struct Accepted {
    pub(crate) io: BoxIo,
    pub(crate) peer: Peer,
    pub(crate) proto: Proto,
}

/// How an accepted connection becomes a served one.
pub(crate) enum Acceptor {
    /// The TLS terminator: what the proxy runs with.
    Tls(Arc<TlsReloader>),
    /// HTTP/2 in the clear, for the tests that exercise routing, holding and auth and need no
    /// handshake; the identity it gives every connection is the one the test wants a
    /// verified certificate to have had. It is not compiled into the binary.
    #[cfg(test)]
    Plain(Option<Identity>),
}

impl Acceptor {
    /// Runs the handshake of `io` from `ip` within `limit`. `None`: the connection is closed
    /// (a failed or slow handshake, or a certificate that names no usable identity), and the
    /// reason is counted.
    pub(crate) async fn accept(
        &self,
        io: BoxIo,
        ip: IpAddr,
        limit: Duration,
        metrics: &Metrics,
    ) -> Option<Accepted> {
        match self {
            Self::Tls(reloader) => {
                let acceptor = TlsAcceptor::from(reloader.config());
                let Ok(Ok(tls)) = tokio::time::timeout(limit, acceptor.accept(io)).await else {
                    Metrics::inc(&metrics.handshake_failed);
                    return None;
                };
                let (_, session) = tls.get_ref();
                let proto = match session.alpn_protocol() {
                    Some(b"h2") => Proto::H2,
                    _ => Proto::H1,
                };
                let identity = match session.peer_certificates().and_then(<[_]>::first) {
                    None => None,
                    Some(leaf) => {
                        let Ok(identity) = identity_of(leaf.as_ref()) else {
                            Metrics::inc(&metrics.identity_refused);
                            return None;
                        };
                        Some(identity)
                    }
                };
                Some(Accepted {
                    io: Box::new(tls),
                    peer: Peer {
                        identity,
                        ip,
                        h1: proto == Proto::H1,
                    },
                    proto,
                })
            }
            #[cfg(test)]
            Self::Plain(identity) => {
                let _ = (limit, metrics);
                Some(Accepted {
                    io,
                    peer: Peer {
                        identity: identity.clone(),
                        ip,
                        h1: false,
                    },
                    proto: Proto::H2,
                })
            }
        }
    }
}
