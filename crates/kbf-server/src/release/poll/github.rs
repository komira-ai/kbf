//! The poller's one request: `GET /repos/<repo>/releases` on the GitHub API, conditional on
//! the `ETag` of the last answer.
//!
//! No credential is ever sent (phase 1 reads public releases). One connection per request
//! over TCP or rustls, no redirects (an API that redirects is an error to report, not a
//! host to follow), the body read to a limit, and the whole call under one timeout. Written
//! over hyper's connection API: the server already links hyper through tonic and axum.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty, Limited};
use hyper::Request;
use hyper::header::{ACCEPT, CONNECTION, HOST, HeaderMap, IF_NONE_MATCH, USER_AGENT};
use hyper::http::uri::{Authority, Scheme};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;
use tokio_rustls::TlsConnector;

use super::{FetchError, Fetched, Source};

/// The most bytes of a feed that are read: thirty releases with their notes and assets are
/// a few hundred kilobytes.
const BODY_MAX: usize = 4 * 1024 * 1024;

/// The roots a TLS connection trusts.
#[derive(Clone, Debug)]
pub enum Roots {
    /// The host's system store.
    System,
    /// Exactly these.
    Given(Vec<CertificateDer<'static>>),
}

/// The GitHub Releases API of one repository.
pub struct GithubSource {
    scheme: Scheme,
    authority: Authority,
    path: String,
    timeout: Duration,
    roots: Roots,
    tls: OnceCell<Result<TlsConnector, String>>,
}

impl std::fmt::Debug for GithubSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubSource")
            .field("authority", &self.authority)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl GithubSource {
    /// The releases of `repo` (`owner/name`) at the API `api_url` (`https://api.github.com`;
    /// a plain `http` URL is for a fake on loopback), each call bounded by `timeout`.
    ///
    /// # Errors
    /// `api_url` is not an `http` or `https` URL with a host.
    pub fn new(api_url: &str, repo: &str, timeout: Duration, roots: Roots) -> Result<Self, String> {
        let uri: hyper::Uri = api_url.parse().map_err(|e| format!("{api_url}: {e}"))?;
        let scheme = uri
            .scheme()
            .cloned()
            .filter(|s| *s == Scheme::HTTP || *s == Scheme::HTTPS);
        let (Some(scheme), Some(authority)) = (scheme, uri.authority().cloned()) else {
            return Err(format!("{api_url}: not an http or https URL with a host"));
        };
        Ok(Self {
            scheme,
            authority,
            path: format!("/repos/{repo}/releases?per_page=30"),
            timeout,
            roots,
            tls: OnceCell::new(),
        })
    }

    /// The TLS client, made on first use. Loading the system store reads files, so it runs
    /// on the blocking pool, never on the runtime's workers.
    async fn connector(&self) -> Result<&TlsConnector, FetchError> {
        let made = self
            .tls
            .get_or_init(|| async {
                let certs = match &self.roots {
                    Roots::System => tokio::task::spawn_blocking(|| {
                        rustls_native_certs::load_native_certs().certs
                    })
                    .await
                    .map_err(|e| format!("loading the system roots: {e}"))?,
                    Roots::Given(certs) => certs.clone(),
                };
                let mut store = rustls::RootCertStore::empty();
                let (added, _ignored) = store.add_parsable_certificates(certs);
                if added == 0 {
                    return Err("no trusted root certificates".to_owned());
                }
                let provider = Arc::new(rustls::crypto::ring::default_provider());
                let mut config = rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .expect("the ring provider supports the default TLS versions")
                    .with_root_certificates(store)
                    .with_no_client_auth();
                config.alpn_protocols = vec![b"http/1.1".to_vec()];
                Ok(TlsConnector::from(Arc::new(config)))
            })
            .await;
        made.as_ref()
            .map_err(|why| FetchError::Transport(format!("TLS: {why}")))
    }

    async fn get(&self, etag: Option<&str>) -> Result<Fetched, FetchError> {
        let https = self.scheme == Scheme::HTTPS;
        let host = self.authority.host();
        let port = self
            .authority
            .port_u16()
            .unwrap_or(if https { 443 } else { 80 });
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| FetchError::Transport(format!("connect: {e}")))?;
        let response = if https {
            let name = ServerName::try_from(host.to_owned())
                .map_err(|e| FetchError::Transport(e.to_string()))?;
            let stream = self
                .connector()
                .await?
                .connect(name, tcp)
                .await
                .map_err(|e| FetchError::Transport(format!("TLS: {e}")))?;
            self.send(TokioIo::new(stream), etag).await?
        } else {
            self.send(TokioIo::new(tcp), etag).await?
        };
        let (head, body) = response.into_parts();
        let status = head.status.as_u16();
        match status {
            304 => Ok(Fetched::NotModified),
            200 => {
                let etag = head
                    .headers
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let body = Limited::new(body, BODY_MAX)
                    .collect()
                    .await
                    .map_err(|e| {
                        if e.downcast_ref::<http_body_util::LengthLimitError>()
                            .is_some()
                        {
                            FetchError::TooLarge
                        } else {
                            FetchError::Transport(format!("body: {e}"))
                        }
                    })?
                    .to_bytes();
                Ok(Fetched::Feed {
                    etag,
                    body: body.to_vec(),
                })
            }
            429 => Err(FetchError::RateLimited(retry_after(&head.headers))),
            403 if head
                .headers
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0") =>
            {
                Err(FetchError::RateLimited(retry_after(&head.headers)))
            }
            other => Err(FetchError::Status(other)),
        }
    }

    async fn send<I>(
        &self,
        io: TokioIo<I>,
        etag: Option<&str>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, FetchError>
    where
        I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let transport = |e: hyper::Error| FetchError::Transport(e.to_string());
        let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(transport)?;
        // It ends with the response; an error then is the server closing early, which the
        // body read reports.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request = Request::get(self.path.as_str())
            .header(HOST, self.authority.as_str())
            .header(USER_AGENT, "kbf-release-poller")
            .header(ACCEPT, "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .header(CONNECTION, "close");
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let request = request
            .body(Empty::<Bytes>::new())
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        sender.send_request(request).await.map_err(transport)
    }
}

impl Source for GithubSource {
    async fn fetch(&self, etag: Option<&str>) -> Result<Fetched, FetchError> {
        tokio::time::timeout(self.timeout, self.get(etag))
            .await
            .unwrap_or(Err(FetchError::Timeout))
    }
}

/// How long the answer asks the caller to wait: `Retry-After` in seconds, else the time
/// until `X-RateLimit-Reset` (seconds since the epoch). `None` when neither is a future wait.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    if let Some(secs) = number("retry-after") {
        return Some(Duration::from_secs(secs));
    }
    let reset = number("x-ratelimit-reset")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    reset.checked_sub(now).map(Duration::from_secs)
}
