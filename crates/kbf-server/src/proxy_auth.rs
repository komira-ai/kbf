//! Who a call is when a TLS-terminating proxy forwarded it (`docs/design/frontend-proxy.md`,
//! section 3, "Identity to the child" and "Operator-API trust").
//!
//! **The flag.** `--proxy-auth-file` names a per-boot secret: a file of exactly 32 bytes
//! ([`SECRET_BYTES`]), owned by the server's user with mode 0600 or 0400 (the checks of
//! [`crate::token`]), read once at start and never taken from argv or the environment. The
//! proxy presents it as `x-kbf-proxy-auth: <the 32 bytes as 64 lower-case hex digits>` (a
//! header value cannot hold raw bytes). Only a digest is kept; a presented value is hashed
//! and the digests compared in constant time ([`ProxySecret::admits`]).
//!
//! **The decision** is one pure function, [`classify`], over the peer address, the headers
//! and the configured secret:
//!
//! | secret configured | peer | `x-kbf-proxy-auth` | result |
//! |---|---|---|---|
//! | no | any | any | [`Trust::Direct`]: identity comes from the peer certificate, as before |
//! | yes | not loopback, or unknown | any | refused ([`Refusal::NotLoopback`]) |
//! | yes | loopback | absent or wrong | [`Trust::Unverified`]: the certificate, if any, and nothing from the headers |
//! | yes | loopback | the secret | [`Trust::Proxied`] with the [`Identity`] of `x-kbf-node-id` and `x-kbf-role` (each optional: none is anonymous), or refused ([`Refusal::Malformed`]) |
//!
//! With the flag set the server is behind the proxy, so a call that reaches it from a
//! remote address is refused (gRPC `UNAUTHENTICATED`, HTTP 403) before anything reads it.
//! A local process without the secret cannot claim a node id or a role.
//!
//! **The layers** ([`ProxyAuthLayer`] on the gRPC listeners, [`gate_requests`] on the
//! operator API) apply [`classify`] to every call, strip every `x-kbf-*` header from it
//! whatever the result (so no handler can read one), and put the [`Trust`] in the request's
//! extensions. What reads who the caller is takes the [`Trust`] (an [`Identity`] when
//! proxied) from there, or the certificate when it is not proxied: see
//! [`crate::identity::presented`], the one function that decides which. Every listener the
//! server opens has the layer, with or without the flag, so no `x-kbf-*` header ever reaches
//! a handler.

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use futures::future::{Either, Ready, ready};
use kbf_peer::PeerRole;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;
use tonic::Status;
use tonic::codegen::http::request::Parts;
use tonic::codegen::http::{self, HeaderMap, StatusCode};
use tonic::codegen::{Service, http::Extensions};
use tonic::transport::server::TcpConnectInfo;
use tower_layer::Layer;

use crate::token::{TokenFileError, read_owner_only};

/// The header the proxy presents the secret in.
pub const PROXY_AUTH: &str = "x-kbf-proxy-auth";
/// The header carrying the node a verified certificate names.
pub const NODE_ID: &str = "x-kbf-node-id";
/// The header carrying the role a verified certificate names.
pub const ROLE: &str = "x-kbf-role";
/// What every header the server reserves for itself starts with.
const RESERVED: &str = "x-kbf-";

/// The exact size of the secret file, in bytes.
pub const SECRET_BYTES: usize = 32;

/// The per-boot secret the proxy presents, kept as a digest of the header value.
#[derive(Clone)]
pub struct ProxySecret([u8; 32]);

impl fmt::Debug for ProxySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxySecret(..)")
    }
}

/// Why `--proxy-auth-file` is refused.
#[derive(Debug, thiserror::Error)]
pub enum ProxySecretError {
    /// It cannot be read, or is not a regular file owned by this user with mode 0600 or 0400.
    #[error(transparent)]
    File(#[from] TokenFileError),
    /// It does not hold exactly [`SECRET_BYTES`] bytes.
    #[error("{path} must hold exactly {SECRET_BYTES} bytes")]
    Length { path: PathBuf },
}

impl ProxySecret {
    /// The secret in the file at `path`.
    ///
    /// # Errors
    /// The file cannot be read, is not a regular file, has another owner or a mode other
    /// than 0600 or 0400, or does not hold exactly [`SECRET_BYTES`] bytes.
    #[cfg(unix)]
    pub fn from_file(path: &Path) -> Result<Self, ProxySecretError> {
        let bytes = read_owner_only(path, SECRET_BYTES)?;
        if bytes.len() != SECRET_BYTES {
            return Err(ProxySecretError::Length {
                path: path.to_owned(),
            });
        }
        Ok(Self(Sha256::digest(hex::encode(bytes)).into()))
    }

    /// Elsewhere than Unix the file's owner and mode cannot be checked, so none is accepted.
    ///
    /// # Errors
    /// Always.
    #[cfg(not(unix))]
    pub fn from_file(path: &Path) -> Result<Self, ProxySecretError> {
        Err(ProxySecretError::File(TokenFileError::Read {
            path: path.to_owned(),
            source: std::io::ErrorKind::Unsupported.into(),
        }))
    }

    /// Whether `presented`, an `x-kbf-proxy-auth` value, is the secret: the digests of the two
    /// compared in constant time, so neither the bytes nor the length show through timing.
    #[must_use]
    pub fn admits(&self, presented: &[u8]) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented).into();
        bool::from(presented.ct_eq(&self.0))
    }
}

/// What a proxy says it verified about a caller: the node its certificate names and the
/// role it names, each absent when the caller presented none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// From `x-kbf-node-id`.
    pub node_id: Option<String>,
    /// From `x-kbf-role`.
    pub role: Option<PeerRole>,
}

/// How far a call's identity headers are to be believed. The layers put it in the request's
/// extensions; a request with none is [`Trust::Direct`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Trust {
    /// No proxy is configured: identity is the peer certificate's.
    #[default]
    Direct,
    /// A proxy is configured and the caller, on loopback, did not present its secret:
    /// identity is the peer certificate's, if there is one, and nothing from the headers.
    Unverified,
    /// A proxy is configured and the caller presented its secret.
    Proxied(Identity),
}

/// Why a call is refused before it is read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// A proxy is configured and the call did not come from this host.
    #[error(
        "this server is behind a proxy and takes calls from its own host only; \
         it was reached directly"
    )]
    NotLoopback,
    /// The proxy's identity header is not one well-formed value.
    #[error("{0} is not a single well-formed value")]
    Malformed(&'static str),
}

impl Refusal {
    /// The gRPC status: `UNAUTHENTICATED` for a call that came the wrong way,
    /// `PERMISSION_DENIED` for a malformed identity.
    #[must_use]
    pub fn status(&self) -> Status {
        match self {
            Self::NotLoopback => Status::unauthenticated(self.to_string()),
            Self::Malformed(_) => Status::permission_denied(self.to_string()),
        }
    }
}

/// How far to believe the headers of a call from `peer` (`None`: its address is unknown),
/// under the table in the module docs. Pure: it reads nothing but its arguments.
///
/// # Errors
/// [`Refusal::NotLoopback`] when a secret is configured and `peer` is not loopback;
/// [`Refusal::Malformed`] when the secret is presented with an identity header that is not
/// one well-formed value.
pub fn classify(
    secret: Option<&ProxySecret>,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
) -> Result<Trust, Refusal> {
    let Some(secret) = secret else {
        return Ok(Trust::Direct);
    };
    if !peer.is_some_and(|peer| peer.ip().to_canonical().is_loopback()) {
        return Err(Refusal::NotLoopback);
    }
    let presented = headers.get(PROXY_AUTH).map(|v| v.as_bytes());
    if !presented.is_some_and(|presented| secret.admits(presented)) {
        return Ok(Trust::Unverified);
    }
    Ok(Trust::Proxied(Identity {
        node_id: single(headers, NODE_ID)?.map(str::to_owned),
        role: single(headers, ROLE)?
            .map(|name| PeerRole::from_name(name).ok_or(Refusal::Malformed(ROLE)))
            .transpose()?,
    }))
}

/// The one non-empty visible-ASCII value of `name`, or `None` if the header is absent.
fn single<'h>(headers: &'h HeaderMap, name: &'static str) -> Result<Option<&'h str>, Refusal> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    match first.to_str() {
        Ok(text) if !text.is_empty() && values.next().is_none() => Ok(Some(text)),
        _ => Err(Refusal::Malformed(name)),
    }
}

/// Removes every `x-kbf-*` header from `headers`: what a caller sent under a name the
/// server reserves is never read.
pub fn strip(headers: &mut HeaderMap) {
    let reserved: Vec<_> = headers
        .keys()
        .filter(|name| name.as_str().starts_with(RESERVED))
        .cloned()
        .collect();
    for name in reserved {
        headers.remove(name);
    }
}

/// The address of the caller of a call, from a connection of [`kbf_tls::Incoming`] or of
/// tonic's own TCP incoming.
fn peer_addr(extensions: &Extensions) -> Option<SocketAddr> {
    let tcp = match extensions.get::<kbf_tls::ConnInfo>() {
        Some(info) => Some(&info.tcp),
        None => extensions.get::<TcpConnectInfo>(),
    };
    tcp.and_then(|tcp| tcp.remote_addr)
}

/// Applies [`classify`] to every call of the gRPC server it is added to (with
/// `Server::layer`, last so that it runs first), strips its `x-kbf-*` headers and records
/// the [`Trust`]; a refused call is answered with its status and never reaches a service.
#[derive(Clone, Debug)]
pub struct ProxyAuthLayer {
    secret: Option<Arc<ProxySecret>>,
}

impl ProxyAuthLayer {
    /// The layer for a listener behind the proxy whose secret is `secret` (`None`: no proxy).
    #[must_use]
    pub const fn new(secret: Option<Arc<ProxySecret>>) -> Self {
        Self { secret }
    }
}

impl<S> Layer<S> for ProxyAuthLayer {
    type Service = ProxyAuth<S>;

    fn layer(&self, inner: S) -> ProxyAuth<S> {
        ProxyAuth {
            inner,
            secret: self.secret.clone(),
        }
    }
}

/// The service [`ProxyAuthLayer`] wraps around a gRPC listener's routes.
#[derive(Clone, Debug)]
pub struct ProxyAuth<S> {
    inner: S,
    secret: Option<Arc<ProxySecret>>,
}

impl<S, B, R> Service<http::Request<B>> for ProxyAuth<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>>,
    R: Default,
{
    type Response = http::Response<R>;
    type Error = S::Error;
    type Future = Either<Ready<Result<Self::Response, Self::Error>>, S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let peer = peer_addr(request.extensions());
        match classify(self.secret.as_deref(), peer, request.headers()) {
            Ok(trust) => {
                strip(request.headers_mut());
                request.extensions_mut().insert(trust);
                Either::Right(self.inner.call(request))
            }
            Err(refusal) => {
                tracing::warn!(call = request.uri().path(), %refusal, "call refused");
                Either::Left(ready(Ok(refusal.status().into_http())))
            }
        }
    }
}

/// The operator API's counterpart of [`ProxyAuthLayer`], for
/// `axum::middleware::from_fn_with_state`: the same decision on the peer address axum took
/// from the connection ([`ConnectInfo`]), answered with 403.
pub async fn gate_requests(
    State(secret): State<Option<Arc<ProxySecret>>>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    match classify(secret.as_deref(), peer, request.headers()) {
        Ok(trust) => {
            strip(request.headers_mut());
            request.extensions_mut().insert(trust);
            next.run(request).await
        }
        Err(refusal) => crate::api::error(StatusCode::FORBIDDEN, &refusal.to_string()),
    }
}

/// Who called an operator API route: the address of the connection and how far the proxy
/// vouches for it. Taken from the request by the handlers in place of `ConnectInfo`.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The peer address of the connection.
    pub addr: SocketAddr,
    /// What [`gate_requests`] decided about it.
    pub trust: Trust,
}

impl Caller {
    /// A caller on a server with no proxy configured.
    #[must_use]
    pub const fn direct(addr: SocketAddr) -> Self {
        Self {
            addr,
            trust: Trust::Direct,
        }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = <ConnectInfo<SocketAddr> as FromRequestParts<S>>::Rejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let trust = parts.extensions.get::<Trust>().cloned().unwrap_or_default();
        ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map(|ConnectInfo(addr)| Self { addr, trust })
    }
}
