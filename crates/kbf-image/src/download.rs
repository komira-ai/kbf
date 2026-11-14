//! [`Downloader`]: a plain GET of a URL, to memory or to a file, over the client the registry
//! reads use (hyper over rustls, the host's roots, redirects followed, every wait bounded).
//!
//! It checks no digest: the caller hashes what it keeps. A status that is not success, a body
//! that ends before its `Content-Length` and a stall are errors, so a file written here is
//! whole as far as the transport can say, and the caller's digest settles the rest. Plain
//! `http://` is spoken only to a loopback address (the tests' servers); every other URL is
//! https.

use std::path::Path;

use tokio::io::AsyncWriteExt as _;

use crate::Error;
use crate::http::{Http, Roots, Timeouts};

/// Downloads with the host's trusted roots.
#[derive(Clone, Copy, Debug)]
pub struct Downloader {
    timeouts: Timeouts,
}

impl Downloader {
    /// A downloader whose connects, response heads and body stalls wait as `timeouts` say.
    #[must_use]
    pub fn new(timeouts: Timeouts) -> Self {
        Self { timeouts }
    }

    /// The client for `url`: plain http is allowed to its authority when that is loopback.
    fn client(&self, url: &str) -> Http {
        let mut http = Http::new(Roots::System);
        http.timeouts = self.timeouts;
        if let Some(authority) = loopback_authority(url) {
            http.allow_plain(authority);
        }
        http
    }

    /// The body of `url`, at most `limit` bytes.
    ///
    /// # Errors
    /// The request failed, the answer is not success, or the body is longer than `limit`.
    pub async fn bytes(&self, url: &str, limit: u64) -> Result<Vec<u8>, Error> {
        let http = self.client(url);
        let response = http.get(url, &[]).await?;
        if !response.status().is_success() {
            return Err(Error::Status {
                url: url.to_owned(),
                status: response.status().as_u16(),
            });
        }
        http.read_limited(url, response, limit).await
    }

    /// Writes the body of `url` to `to`, a new file, a chunk at a time: the body is never held.
    /// Returns the number of bytes written. A failure leaves `to` as far as it got; the caller
    /// discards it.
    ///
    /// # Errors
    /// The request failed, the answer is not success, the body ended early or stalled, or the
    /// file could not be written.
    pub async fn file(&self, url: &str, to: &Path) -> Result<u64, Error> {
        let http = self.client(url);
        let response = http.get(url, &[]).await?;
        if !response.status().is_success() {
            return Err(Error::Status {
                url: url.to_owned(),
                status: response.status().as_u16(),
            });
        }
        let io = |source| Error::Io {
            path: to.to_owned(),
            source,
        };
        let mut file = tokio::fs::File::create(to).await.map_err(io)?;
        let mut body = response.into_body();
        let mut written = 0_u64;
        while let Some(chunk) = http.chunk(url, &mut body).await? {
            file.write_all(&chunk).await.map_err(io)?;
            written += chunk.len() as u64;
        }
        file.flush().await.map_err(io)?;
        Ok(written)
    }
}

/// The `host:port` of `url` when it is `http://` on a loopback host.
fn loopback_authority(url: &str) -> Option<String> {
    let uri: hyper::Uri = url.parse().ok()?;
    let loopback = matches!(uri.host()?, "127.0.0.1" | "localhost" | "[::1]");
    (uri.scheme_str() == Some("http") && loopback)
        .then(|| uri.authority().map(|a| a.to_string()))?
}
