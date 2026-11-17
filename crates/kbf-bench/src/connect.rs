//! The connection to the blob service: one HTTP/2 channel, made as a transfer client
//! makes its connection (`Endpoint::from_shared(url).tls_config(..)`, then
//! `kbf_transfer::Transfers::new`: the engine's default stream window, eight times it
//! for the connection, `connect_lazy()`), with TLS flags that load a CA and, when a client
//! certificate is given, a client identity. Plain text is allowed, for a REAPI listener
//! served so.

use std::path::{Path, PathBuf};

use kbf_transfer::TransferConfig;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// Where the blobs go, and how to authenticate.
#[derive(Clone, Debug, clap::Args)]
pub struct Target {
    /// The blob service: a kbf-server REAPI listener (`https://`, or `http://` when it
    /// serves plain text).
    #[arg(long)]
    pub cas: String,
    #[command(flatten)]
    pub tls: Tls,
}

/// The TLS flags: how to check the server, and how to present a client.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct Tls {
    /// PEM file of the CA certificates that sign the server's certificate (`https://`
    /// only; required there).
    #[arg(long)]
    pub ca_cert: Option<PathBuf>,
    /// PEM file of the client certificate chain, for a listener that asks for one.
    #[arg(long, requires = "key")]
    pub cert: Option<PathBuf>,
    /// PEM file of the client's private key.
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
    #[error("{flag}: read {path}: {source}")]
    Read {
        /// The flag that named the file.
        flag: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
    channel_to(target.cas.clone(), &target.tls)
}

/// A channel to `url` with the TLS flags `flags`, as [`channel`] makes it: the one
/// connection of a `noop` arm too.
///
/// # Errors
/// As [`channel`].
pub fn channel_to(url: String, flags: &Tls) -> Result<Channel, ConnectError> {
    let plain = url.starts_with("http://");
    let tls_flags =
        flags.ca_cert.is_some() | flags.cert.is_some() | flags.tls_server_name.is_some();
    if plain & tls_flags {
        return Err(ConnectError::Usage(
            url,
            "TLS flags given for a plain-text http:// URL",
        ));
    }
    // The windows `Transfers::new` sets with the default stream window.
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
    let Some(ca_cert) = flags.ca_cert.clone() else {
        return Err(ConnectError::Usage(url, "an https:// URL needs --ca-cert"));
    };
    let tls = client_tls(
        &ca_cert,
        flags.cert.as_deref().zip(flags.key.as_deref()),
        flags.tls_server_name.clone(),
    )?;
    let endpoint = endpoint
        .tls_config(tls)
        .map_err(|e| ConnectError::Transport(url, e))?;
    Ok(endpoint.connect_lazy())
}

/// TLS that trusts only the CA in `ca_cert`, presents the identity of the certificate chain
/// and key files `identity` when there is one, and verifies the server against `name` when
/// there is one (else against the URL's host).
fn client_tls(
    ca_cert: &Path,
    identity: Option<(&Path, &Path)>,
    name: Option<String>,
) -> Result<ClientTlsConfig, ConnectError> {
    let tls =
        ClientTlsConfig::new().ca_certificate(Certificate::from_pem(read("--ca-cert", ca_cert)?));
    let tls = match identity {
        Some((cert, key)) => tls.identity(Identity::from_pem(
            read("--cert", cert)?,
            read("--key", key)?,
        )),
        None => tls,
    };
    Ok(name.into_iter().fold(tls, ClientTlsConfig::domain_name))
}

fn read(flag: &'static str, path: &Path) -> Result<Vec<u8>, ConnectError> {
    std::fs::read(path).map_err(|source| ConnectError::Read {
        flag,
        path: path.to_owned(),
        source,
    })
}
