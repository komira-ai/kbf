//! The client: one HTTP/1.1 exchange per call, over TCP or over TLS pinned to one CA.
//!
//! Every call opens its own connection and asks for it to close (`Connection: close`). The
//! lease server answers a few requests a second per host, a handshake on a farm network is
//! cheap beside the heartbeat the server's answers trail by, and a connection that is never
//! reused cannot go stale between an idle gap and the next call (the server drops a
//! connection idle for its header timeout, 10 s).

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::config::{Config, Origin, Token};
use crate::error::{ApiError, ConfigError, Error, ErrorCode};
use crate::ids::{LeaseId, RunId};
use crate::wire::{
    AcquireRequest, Acquired, ErrorBody, Granted, Host, HostList, Lease, LeaseList, PendingLease,
    ReleaseState, Released, RenewRequest, Renewed, Run, RunAccepted, RunList, RunSpec,
};

/// The most a response body may hold. A host list of a big farm is a few hundred KiB.
const MAX_BODY: usize = 16 << 20;

/// A call ran out of time.
#[derive(Debug, thiserror::Error)]
#[error("timed out {0}")]
struct TimedOut(&'static str);

fn unreachable(cause: impl Into<Box<dyn StdError + Send + Sync>>) -> Error {
    Error::Unreachable(cause.into())
}

fn transport(cause: impl Into<Box<dyn StdError + Send + Sync>>) -> Error {
    Error::Transport(cause.into())
}

/// A client of one lease server. Cheap to clone; the clones share nothing mutable, so any
/// number of calls may be in flight at once.
#[derive(Clone)]
pub struct Client {
    origin: Origin,
    token: Token,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("origin", &self.origin.authority())
            .field("tls", &self.origin.is_tls())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for `config`.
    ///
    /// # Errors
    /// An `https` origin has no CA, or its CA holds no usable certificate, or an `http`
    /// origin has a CA.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let Config {
            origin,
            token,
            ca_pem,
            connect_timeout,
            request_timeout,
        } = config;
        let tls = match (origin.is_tls(), ca_pem) {
            (true, Some(pem)) => Some(connector(&origin, &pem)?),
            (true, None) => return Err(ConfigError::MissingCa),
            (false, Some(_)) => return Err(ConfigError::UnusedCa),
            (false, None) => None,
        };
        Ok(Self {
            origin,
            token,
            tls,
            connect_timeout,
            request_timeout,
        })
    }

    /// Asks for a lease (`POST /v1/leases`). 201 is a lease that is held; 202 is one booked
    /// on a host that is not ready yet, which `leases` shows `held` once it is.
    ///
    /// # Errors
    /// `409 no-capacity` when no free host fits, `422` for a request that breaks a rule of
    /// the class, and the transport errors.
    pub async fn acquire(&self, request: &AcquireRequest) -> Result<Acquired, Error> {
        let (status, body) = self
            .call(
                Method::POST,
                "/v1/leases".to_owned(),
                Some(request),
                &[201, 202],
            )
            .await?;
        match status {
            201 => Ok(Acquired::Granted(decode::<Granted>(status, &body)?)),
            _ => Ok(Acquired::Pending(decode::<PendingLease>(status, &body)?)),
        }
    }

    /// Renews a lease (`POST /v1/leases/{id}/renew`) for `ttl`, or the server's default ttl.
    /// The new expiry is clamped to the server's maximum ttl and the lease's end.
    ///
    /// # Errors
    /// `404 unknown-lease` when the server has forgotten the lease, `409 not-held` or
    /// `409 expired` when it is no longer in force.
    pub async fn renew(&self, lease: LeaseId, ttl: Option<Duration>) -> Result<Renewed, Error> {
        let body = RenewRequest {
            ttl_seconds: ttl.map(|ttl| ttl.as_secs()),
        };
        let path = format!("/v1/leases/{lease}/renew");
        let (status, body) = self.call(Method::POST, path, Some(&body), &[200]).await?;
        decode(status, &body)
    }

    /// Releases a lease (`DELETE /v1/leases/{id}`); idempotent. The server asks every live
    /// run of the lease to stop. Read the results first: once the lease is gone, its runs
    /// are `404 unknown-lease`.
    ///
    /// # Errors
    /// `404 unknown-lease` for a lease the server does not have.
    pub async fn release(&self, lease: LeaseId) -> Result<ReleaseState, Error> {
        let path = format!("/v1/leases/{lease}");
        let (status, body) = self
            .call::<()>(Method::DELETE, path, None, &[200, 202])
            .await?;
        Ok(decode::<Released>(status, &body)?.state)
    }

    /// Every lease the server holds, whoever asked for it (`GET /v1/leases`).
    ///
    /// # Errors
    /// The transport errors.
    pub async fn leases(&self) -> Result<Vec<Lease>, Error> {
        let path = "/v1/leases".to_owned();
        let (status, body) = self.call::<()>(Method::GET, path, None, &[200]).await?;
        Ok(decode::<LeaseList>(status, &body)?.leases)
    }

    /// The leases that carry the label `key=value`: how a restarted caller finds the leases
    /// it made before.
    ///
    /// # Errors
    /// As [`Client::leases`].
    pub async fn leases_labelled(&self, key: &str, value: &str) -> Result<Vec<Lease>, Error> {
        let mut leases = self.leases().await?;
        leases.retain(|lease| lease.labels.get(key).map(String::as_str) == Some(value));
        Ok(leases)
    }

    /// The hosts the server leases, with their liveness and taint (`GET /v1/hosts`).
    ///
    /// # Errors
    /// The transport errors.
    pub async fn hosts(&self) -> Result<Vec<Host>, Error> {
        let path = "/v1/hosts".to_owned();
        let (status, body) = self.call::<()>(Method::GET, path, None, &[200]).await?;
        Ok(decode::<HostList>(status, &body)?.hosts)
    }

    /// Queues a run on the host the lease holds (`POST /v1/leases/{id}/runs`). The server
    /// mints the run id, and the call is **not idempotent**: an [`Error`] for which
    /// [`Error::may_have_applied`] is true leaves the run possibly queued, and asking again
    /// may run it twice.
    ///
    /// # Errors
    /// `409 no-capacity` while the booked runs have not ended (retry later: limits come back
    /// two or three heartbeats after a run exits), `409 not-held`, `host-lost`,
    /// `host-not-registered`, `host-tainted` and `not-ready`, and `422` for a job that
    /// breaks the run contract (`image-digest-required`, `limits-exceed-shape`, ...).
    pub async fn queue_run(&self, lease: LeaseId, run: &RunSpec) -> Result<RunAccepted, Error> {
        let path = format!("/v1/leases/{lease}/runs");
        let (status, body) = self.call(Method::POST, path, Some(run), &[202]).await?;
        decode(status, &body)
    }

    /// One run (`GET /v1/leases/{id}/runs/{run_id}`).
    ///
    /// # Errors
    /// `404 unknown-run`, `404 unknown-lease`, and `410 run-forgotten` for a run the server
    /// lost by restarting: the work is gone, and the caller submits it again as a new run.
    pub async fn run(&self, lease: LeaseId, run: &RunId) -> Result<Run, Error> {
        let path = format!("/v1/leases/{lease}/runs/{run}");
        let (status, body) = self.call::<()>(Method::GET, path, None, &[200]).await?;
        decode(status, &body)
    }

    /// The runs of a lease, oldest first (`GET /v1/leases/{id}/runs`).
    ///
    /// # Errors
    /// `404 unknown-lease`.
    pub async fn runs(&self, lease: LeaseId) -> Result<Vec<Run>, Error> {
        let path = format!("/v1/leases/{lease}/runs");
        let (status, body) = self.call::<()>(Method::GET, path, None, &[200]).await?;
        Ok(decode::<RunList>(status, &body)?.runs)
    }

    /// Asks for a run to stop (`DELETE /v1/leases/{id}/runs/{run_id}`): a retained intent,
    /// carried on every heartbeat reply until the host no longer lists the run. The answer
    /// is 202 whatever the run's state.
    ///
    /// # Errors
    /// As [`Client::run`].
    pub async fn stop_run(&self, lease: LeaseId, run: &RunId) -> Result<RunAccepted, Error> {
        let path = format!("/v1/leases/{lease}/runs/{run}");
        let (status, body) = self.call::<()>(Method::DELETE, path, None, &[202]).await?;
        decode(status, &body)
    }

    /// One exchange: connect, send, read the whole answer, and turn a status that is not
    /// one of `ok` into the server's refusal.
    async fn call<B: Serialize>(
        &self,
        method: Method,
        path: String,
        body: Option<&B>,
        ok: &[u16],
    ) -> Result<(u16, Bytes), Error> {
        let payload = body.map_or_else(Bytes::new, |body| Bytes::from(encode(body)));
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, self.origin.authority())
            .header(AUTHORIZATION, self.token.header().clone())
            .header(CONNECTION, "close")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(payload))
            .map_err(unreachable)?;
        let target = (self.origin.host().to_owned(), self.origin.port());
        let tcp = self.connecting(TcpStream::connect(target)).await?;
        let (status, body) = match &self.tls {
            Some((connector, name)) => {
                let tls = self
                    .connecting(connector.connect(name.clone(), tcp))
                    .await?;
                self.exchange(tls, request).await?
            }
            None => self.exchange(tcp, request).await?,
        };
        match ok.contains(&status.as_u16()) {
            true => Ok((status.as_u16(), body)),
            false => Err(refusal(status, &body).into()),
        }
    }

    /// Runs a connect step under the connect timeout; a failure means nothing was sent.
    async fn connecting<T>(
        &self,
        step: impl Future<Output = std::io::Result<T>>,
    ) -> Result<T, Error> {
        match tokio::time::timeout(self.connect_timeout, step).await {
            Ok(connected) => connected.map_err(unreachable),
            Err(_) => Err(unreachable(TimedOut("connecting"))),
        }
    }

    /// Sends the request on `io` and reads the whole answer, under the request timeout.
    async fn exchange<S>(
        &self,
        io: S,
        request: Request<Full<Bytes>>,
    ) -> Result<(StatusCode, Bytes), Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let work = async {
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
                .await
                .map_err(transport)?;
            // The driver ends with the connection: when the answer is read and `sender`
            // drops, or when the server closes.
            tokio::spawn(connection);
            let response = sender.send_request(request).await.map_err(transport)?;
            let (head, body) = response.into_parts();
            let body = Limited::new(body, MAX_BODY)
                .collect()
                .await
                .map_err(transport)?;
            Ok((head.status, body.to_bytes()))
        };
        match tokio::time::timeout(self.request_timeout, work).await {
            Ok(answer) => answer,
            Err(_) => Err(transport(TimedOut("waiting for the answer"))),
        }
    }
}

/// The TLS connector that trusts `pem` and nothing else, and the name the origin's
/// certificate must carry.
fn connector(
    origin: &Origin,
    pem: &[u8],
) -> Result<(TlsConnector, ServerName<'static>), ConfigError> {
    let name = ServerName::try_from(origin.host().to_owned()).map_err(|_| ConfigError::Origin {
        origin: origin.authority().to_owned(),
        reason: "not a name a certificate can carry",
    })?;
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
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok((TlsConnector::from(Arc::new(config)), name))
}

/// The JSON of a request body. The types sent have string keys only, so this does not fail.
fn encode<T: Serialize>(body: &T) -> Vec<u8> {
    serde_json::to_vec(body).unwrap_or_default()
}

fn decode<T: DeserializeOwned>(status: u16, body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|source| Error::Decode { status, source })
}

/// The refusal in an error answer. A body that is not `{"error": ...}` (the 401 has none)
/// is [`ErrorCode::Unspecified`].
fn refusal(status: StatusCode, body: &[u8]) -> ApiError {
    let (code, reason) = match serde_json::from_slice::<ErrorBody>(body) {
        Ok(body) => (ErrorCode::from_text(&body.error), body.reason),
        Err(_) => (ErrorCode::Unspecified, None),
    };
    ApiError {
        status: status.as_u16(),
        code,
        reason,
    }
}
