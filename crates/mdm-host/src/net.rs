//! Downloads, over HTTPS only, for `fetch`.

use std::sync::Arc;
use std::time::Duration;

use rustls::crypto::ring::default_provider;
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::resolver::{DefaultResolver, Resolver};
use ureq::unversioned::transport::{Connector, DefaultConnector};

/// How long one download may take, whole.
const TIMEOUT: Duration = Duration::from_secs(600);

/// Why a download failed.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// The server answered with a status outside 2xx.
    #[error("answered {0}")]
    Status(u16),
    /// The request did not complete: a URL the client refuses (plain `http://` when only
    /// HTTPS is allowed), the connection or TLS failed, or the body passed the limit.
    #[error(transparent)]
    Transport(#[from] ureq::Error),
}

/// Fetches a URL's body.
pub trait Download {
    /// The body of `url`, which must answer 2xx and hold at most `limit` bytes.
    ///
    /// # Errors
    /// [`DownloadError`]: the status, or why the request did not complete.
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, DownloadError>;
}

/// [`Download`] over ureq: redirects are followed (release assets are served from a
/// redirect target), every hop must be `https://` when `https_only`, no proxy setting is
/// read, and the public web roots are trusted.
pub struct Net {
    agent: Agent,
}

impl Net {
    /// An agent; `https_only` is true outside tests.
    pub fn new(https_only: bool) -> Net {
        Net::with_parts(
            https_only,
            DefaultConnector::default(),
            DefaultResolver::default(),
        )
    }

    /// An agent over this connector and resolver (the network's own in [`Net::new`]; an
    /// in-memory pair in this module's tests).
    fn with_parts(https_only: bool, connector: impl Connector, resolver: impl Resolver) -> Net {
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::WebPki)
            .unversioned_rustls_crypto_provider(Arc::new(default_provider()))
            .build();
        let config = Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .https_only(https_only)
            .timeout_global(Some(TIMEOUT))
            .tls_config(tls)
            .build();
        Net {
            agent: Agent::with_parts(config, connector, resolver),
        }
    }
}

impl Download for Net {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, DownloadError> {
        let mut answer = self.agent.get(url).call()?;
        let status = answer.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(DownloadError::Status(status));
        }
        let body = answer.body_mut().with_config().limit(limit);
        Ok(body.read_to_vec()?)
    }
}

#[cfg(test)]
#[path = "net_tests.rs"]
mod tests;
