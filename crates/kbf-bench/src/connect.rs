//! The connection to the blob service: one HTTP/2 channel, made as `kbf-daemon` makes
//! its `--cas` connection (`Endpoint::from_shared(url).tls_config(..)`, then
//! `kbf_transfer::Transfers::new`: the engine's default stream window, eight times it
//! for the connection, `connect_lazy()`), with the daemon's TLS flags and its TLS loader
//! when a client certificate is given. Plain text is allowed, for a REAPI listener
//! served so.

use std::path::PathBuf;

use kbf_daemon::config::TlsFiles;
use kbf_transfer::TransferConfig;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

/// Where the blobs go, and how to authenticate.
#[derive(Clone, Debug, clap::Args)]
pub struct Target {
    /// The blob service: a kbf-server worker listener (`https://`, mutual TLS, as the
    /// daemon's `--cas`) or a REAPI listener (`https://`, or `http://` when it serves
    /// plain text).
    #[arg(long)]
    pub cas: String,
    /// PEM file of the CA certificates that sign the server's certificate (`https://`
    /// only; required there).
    #[arg(long)]
    pub ca_cert: Option<PathBuf>,
    /// PEM file of the client certificate chain, as the daemon's `--cert`: a node
    /// certificate for a worker listener.
    #[arg(long, requires = "key")]
    pub cert: Option<PathBuf>,
    /// PEM file of the client's private key, as the daemon's `--key`.
    #[arg(long, requires = "cert")]
    pub key: Option<PathBuf>,
    /// The name to verify in the server certificate, when it is not the URL's host.
    #[arg(long)]
    pub tls_server_name: Option<String>,
}

/// Why no channel could be made.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("--cas {0:?}: {1}")]
    Usage(String, &'static str),
    #[error("--ca-cert: {0}")]
    Tls(#[from] kbf_daemon::config::ConfigError),
    #[error("--cas {0:?}: {1}")]
    Transport(String, #[source] tonic::transport::Error),
}

/// A channel to `target`, connecting on first use. The TLS files are read and parsed
/// here, so a bad one fails the run before any request.
///
/// # Errors
/// A URL that is neither `http://` nor `https://`; TLS flags with `http://`; `https://`
/// without `--ca-cert`; a TLS file that cannot be read or parsed; a URL tonic refuses.
pub fn channel(target: &Target) -> Result<Channel, ConnectError> {
    let url = target.cas.clone();
    let plain = url.starts_with("http://");
    let tls_flags =
        target.ca_cert.is_some() | target.cert.is_some() | target.tls_server_name.is_some();
    if plain & tls_flags {
        return Err(ConnectError::Usage(
            url,
            "TLS flags given for a plain-text http:// URL",
        ));
    }
    // The windows `Transfers::new` sets for a daemon run with the default
    // `--transfer-stream-window-bytes`.
    let window = TransferConfig::default().stream_window_bytes;
    let endpoint = Endpoint::from_shared(url.clone())
        .map_err(|e| ConnectError::Transport(url.clone(), e))?
        .initial_stream_window_size(window)
        .initial_connection_window_size(window.saturating_mul(8));
    if plain {
        return Ok(endpoint.connect_lazy());
    }
    if !url.starts_with("https://") {
        return Err(ConnectError::Usage(
            url,
            "must be an http:// or https:// URL",
        ));
    }
    let Some(ca_cert) = target.ca_cert.clone() else {
        return Err(ConnectError::Usage(url, "an https:// URL needs --ca-cert"));
    };
    let tls = match (&target.cert, &target.key) {
        (Some(cert), Some(key)) => TlsFiles {
            ca_cert,
            cert: cert.clone(),
            key: key.clone(),
            server_name: target.tls_server_name.clone(),
        }
        .load()?,
        _ => server_only(ca_cert, target.tls_server_name.clone())?,
    };
    let endpoint = endpoint
        .tls_config(tls)
        .map_err(|e| ConnectError::Transport(url, e))?;
    Ok(endpoint.connect_lazy())
}

/// TLS that checks the server and presents no client certificate.
fn server_only(ca_cert: PathBuf, name: Option<String>) -> Result<ClientTlsConfig, ConnectError> {
    let pem = std::fs::read(&ca_cert).map_err(|source| kbf_daemon::config::ConfigError::Read {
        path: ca_cert,
        source,
    })?;
    let tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem));
    Ok(name.into_iter().fold(tls, ClientTlsConfig::domain_name))
}
