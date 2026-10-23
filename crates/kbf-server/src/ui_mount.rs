//! The pages of `kbf-ui` on the operator API, under `/ui`, read-only and open
//! (`docs/design/kbf-ui-v1.md`, R5; the ruling of 2026-10-20 made it open).
//!
//! - **On by default.** `--ui` is on unless `--ui=false`; with the operator API
//!   (`--api-listen`) off there is nothing to mount on. Off, `/ui` is a plain `404`.
//! - **Open: no login, no certificate.** `/ui` asks for no identity and every page is
//!   served to whoever reaches the operator API's port: everything the pages read is shown
//!   unredacted (logs, a failed command's arguments). The write routes and the operator
//!   token are untouched: the UI mounts no route that writes, and its source can only `GET`.
//!   Put the port behind the proxy and the network you trust.
//! - **Read-only.** `GET` and `HEAD` are served; every other method is `405`.
//! - **The same read handlers, asked in process.** A page reads the API through
//!   [`InProcess`]: a `GET` of one of the server's own routes, called as a service on a
//!   router that is not on any listener. Its policy-gated reads (the build pages, the run
//!   logs, the blobs) are built a second time from the same
//!   state with every caller allowed ([`Reads::routes`]), so the REAPI policy's
//!   authenticator and authorizers keep gating the same routes on the network and do not
//!   gate the UI. The in-process request carries the address `127.0.0.1:0` and
//!   [`Trust::Unverified`], the lowest trust, so a handler that asks who called refuses a
//!   write if one were ever reached; none is: the source sends `GET` and a page asks only
//!   for the routes it declares.
//! - **Headers on every `/ui` answer**, errors and `405` included: the
//!   `Content-Security-Policy` of [`kbf_ui::CONTENT_SECURITY_POLICY`] (nothing loads but
//!   the page's own stylesheet; no script; no framing; no `<base>`), `nosniff`,
//!   `Referrer-Policy: no-referrer` (ids sit in URLs), `Cache-Control: no-store` (pages
//!   carry logs), `Cross-Origin-Resource-Policy: same-origin` and
//!   `Cross-Origin-Opener-Policy: same-origin` (no other origin embeds or shares a window
//!   with a page), and a [`PERMISSIONS_POLICY`] denying the powerful features. No HSTS:
//!   TLS ends at the proxy.
//! - **No `Host` check.** A check against the public names guards an identity the page
//!   would otherwise lend to a hostile site (DNS rebinding); `/ui` has none, and what it
//!   shows is open to anyone who can send the request.
//! - **A read budget.** `/ui` and the public read routes each have a [`ReadLimit`] per
//!   caller: [`crate::read_limit`].

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, RANGE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use http_body_util::{BodyExt, Limited};
use kbf_auth::{AllowAuthorizer, Authorizer, Policy};
use kbf_front::{Cache, MetaLog, Recorder};
use kbf_mapsync::MapSource;
use kbf_objstore::ObjectStore;
use kbf_ui::{Fetched, Source, SourceError};
use tower_service::Service;

use crate::history::HistoryReads;
use crate::http_blobs::{BlobReaders, ReadBudget};
use crate::http_invocations::InvocationReaders;
use crate::proxy_auth::Trust;
use crate::read_limit::{PER_CALLER, ReadLimit, limited};
use crate::serve::{Api, Behind, Bound, Listeners, ServeError, bind_server_with_ui};
use crate::state_store::StateStore;

/// [`bind_server_with_policy`](crate::bind_server_with_policy) with all of its options: the
/// scheduler's state in `state` ([`crate::bind_server_with_state`]), the client's CAS,
/// ByteStream and action-cache calls proxied to a `storage` node
/// ([`crate::bind_server_with_storage`]), the storage map `map` sent to daemons
/// ([`crate::bind_server_with_map`]), and a TLS-terminating `proxy` in front of every
/// listener (`--proxy-auth-file`, [`crate::proxy_auth`]; `None`: the server is reached
/// directly). The web UI is on ([`bind_server_with_ui`] turns it off).
///
/// # Errors
/// As [`crate::bind_server_with_state`].
#[expect(
    clippy::too_many_arguments,
    reason = "every option the bind functions delegate"
)]
pub fn bind_server_with_options<M, O>(
    cache: Arc<Cache<M, O>>,
    listeners: Listeners,
    api: Option<Api>,
    policy: Policy,
    state: Option<StateStore>,
    storage: Option<tonic::transport::Endpoint>,
    map: Option<Arc<dyn MapSource>>,
    proxy: Option<Behind>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Bound<impl Future<Output = Result<(), ServeError>>>, ServeError>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    bind_server_with_ui(
        cache, listeners, api, policy, state, storage, map, proxy, true, shutdown,
    )
}

/// The largest answer a page reads from the API: a blob is shown only up to
/// [`crate::http_blobs::INLINE_MAX_BYTES`] and JSON documents are far smaller.
pub const MAX_READ_BYTES: usize = 8 * 1024 * 1024;

/// The address the in-process requests come from.
const LOCAL: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

/// What the policy-gated read routes are built over.
pub(crate) struct Reads<M, O> {
    pub(crate) cache: Arc<Cache<M, O>>,
    pub(crate) recorder: Recorder,
    /// `None` without `--history-objects`.
    pub(crate) history: Option<Arc<HistoryReads>>,
    pub(crate) budget: ReadBudget,
}

impl<M, O> Reads<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    /// The routes `policy` gates: the blob route and the build pages with the run, log and
    /// history routes. Each is off (`404`) where `policy` names no authorizer for it.
    pub(crate) fn routes(&self, policy: &Policy) -> Router {
        let blobs = crate::http_blobs::router(
            Arc::clone(&self.cache),
            BlobReaders::from_policy(policy),
            self.budget.clone(),
        );
        let pages = crate::http_invocations::router(
            self.recorder.clone(),
            Arc::clone(&self.cache),
            InvocationReaders::from_policy(policy),
            self.history.clone(),
            self.budget.clone(),
        );
        blobs.merge(pages)
    }
}

/// A policy that accepts every caller and allows every read: what the pages ask the
/// routes with.
fn open_policy() -> Policy {
    let allow = || -> Arc<dyn Authorizer> { Arc::new(AllowAuthorizer) };
    let mut policy = Policy::allow_all();
    policy.invocations_get = Some(allow());
    policy.blobs_get = Some(allow());
    policy
}

/// `base`, the operator API's routes without the gated reads, with those reads gated by
/// `policy` and bounded per caller; and, when `ui`, the pages under `/ui`.
pub(crate) fn mount<M, O>(base: Router, reads: &Reads<M, O>, policy: &Policy, ui: bool) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let public = reads.routes(policy).layer(middleware::from_fn_with_state(
        ReadLimit::new(PER_CALLER),
        limited,
    ));
    if !ui {
        return base.merge(public);
    }
    let internal = base.clone().merge(reads.routes(&open_policy()));
    base.merge(public).merge(pages(InProcess::new(internal)))
}

/// Asks a router, in process, as a `GET`; what the pages read the API through.
#[derive(Clone)]
pub struct InProcess {
    router: Router,
    max_bytes: usize,
}

impl InProcess {
    /// A source over `router`, reading at most [`MAX_READ_BYTES`] of an answer.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self::bounded(router, MAX_READ_BYTES)
    }

    /// [`Self::new`] reading at most `max_bytes` of an answer.
    #[must_use]
    pub fn bounded(router: Router, max_bytes: usize) -> Self {
        Self { router, max_bytes }
    }
}

impl Source for InProcess {
    async fn get(&self, url: &str) -> Result<Fetched, SourceError> {
        self.ask(url, None).await
    }

    /// The route asked with `Range: bytes=first-last`, so a log or a blob route reads
    /// only those bytes.
    async fn get_range(&self, url: &str, first: u64, last: u64) -> Result<Fetched, SourceError> {
        self.ask(url, Some(format!("bytes={first}-{last}"))).await
    }

    /// The route asked with `Range: bytes=-n`, so a log is read from its end.
    async fn get_suffix(&self, url: &str, n: u64) -> Result<Fetched, SourceError> {
        self.ask(url, Some(format!("bytes=-{n}"))).await
    }
}

impl InProcess {
    /// `GET url`, with `range` as its `Range` header value.
    async fn ask(&self, url: &str, range: Option<String>) -> Result<Fetched, SourceError> {
        let mut request = Request::get(url);
        if let Some(range) = range {
            request = request.header(RANGE, range);
        }
        let mut request = request
            .body(Body::empty())
            .map_err(|e| SourceError(format!("{url:?} is not a request target: {e}")))?;
        // The lowest trust: a handler that decides by who called never takes this for
        // the operator.
        request.extensions_mut().insert(ConnectInfo(LOCAL));
        request.extensions_mut().insert(Trust::Unverified);
        let mut router = self.router.clone();
        // A router's error type is `Infallible`.
        let Ok(answer) = router.call(request).await;
        let (head, body) = answer.into_parts();
        let body = Limited::new(body, self.max_bytes)
            .collect()
            .await
            .map_err(|e| SourceError(format!("the answer to {url} was not read: {e}")))?
            .to_bytes();
        let headers = head
            .headers
            .iter()
            .map(|(name, value)| {
                let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
                (name.as_str().to_owned(), value)
            })
            .collect();
        Ok(Fetched {
            status: head.status.as_u16(),
            body: body.to_vec(),
            headers,
        })
    }
}

/// The `/ui` routes over `source`: `GET` and `HEAD` only, bounded per caller, every
/// answer with the security headers.
pub(crate) fn pages(source: InProcess) -> Router {
    Router::new()
        .route("/ui", get(page))
        .route("/ui/", get(page))
        .route("/ui/{*rest}", get(page))
        .with_state(Arc::new(source))
        .layer(middleware::from_fn_with_state(
            ReadLimit::new(PER_CALLER),
            limited,
        ))
        .layer(middleware::from_fn(secure))
}

async fn page(State(source): State<Arc<InProcess>>, uri: Uri) -> Response {
    let answer = kbf_ui::respond(&*source, uri.path(), uri.query().unwrap_or_default()).await;
    let mut response = Response::new(Body::from(answer.body().to_vec()));
    *response.status_mut() =
        StatusCode::from_u16(answer.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    // A value the API sent and a page passed on is visible ASCII (it came from a header);
    // were one not, it is sent empty.
    let headers = answer.headers().into_iter().map(|(name, value)| {
        let value = HeaderValue::from_str(value).unwrap_or(HeaderValue::from_static(""));
        (HeaderName::from_static(name), value)
    });
    response.headers_mut().extend(headers);
    response
}

/// The `Permissions-Policy` of every `/ui` answer: each powerful feature denied to the
/// page and to any frame (an empty allowlist), so a page that ever carried script could
/// not ask for the camera, the microphone, the location, a USB or serial device, a
/// payment, and the like.
const PERMISSIONS_POLICY: &str = "accelerometer=(), autoplay=(), bluetooth=(), camera=(), \
clipboard-read=(), clipboard-write=(), display-capture=(), encrypted-media=(), \
fullscreen=(), geolocation=(), gyroscope=(), hid=(), idle-detection=(), \
magnetometer=(), microphone=(), midi=(), payment=(), publickey-credentials-get=(), \
screen-wake-lock=(), serial=(), usb=(), web-share=(), xr-spatial-tracking=()";

/// Sets the security headers of every `/ui` answer, over any a handler set.
async fn secure(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (CONTENT_SECURITY_POLICY, kbf_ui::CONTENT_SECURITY_POLICY),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (REFERRER_POLICY, kbf_ui::REFERRER_POLICY),
        (CACHE_CONTROL, "no-store"),
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("permissions-policy"),
            PERMISSIONS_POLICY,
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests;
