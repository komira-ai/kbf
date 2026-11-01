//! The control API for the object-store registration (`docs/api.md`, `/v1/object-stores`):
//!
//! | Route | Does |
//! |---|---|
//! | `PUT /connections/{name}` | create or replace a connection; idempotent |
//! | `GET /connections`, `GET /connections/{name}` | read, never a secret |
//! | `DELETE /connections/{name}` | delete; 409 `in_use` while bound |
//! | `POST /connections/{name}:verify` | probe the store; change nothing |
//! | `POST /connections/{name}:repoint` | move the bound store (endpoint, bucket, prefix); always probed |
//! | `PUT /bindings/{role}` | bind the role `cold` |
//! | `DELETE /bindings/{role}` | unbind; 409 `in_use` once a node has applied it |
//! | `GET /v1/object-stores` | the desired state and the nodes' convergence |
//! | `GET /v1/object-stores/audit?limit=` | the audit rows |
//!
//! **Every route, a GET included, passes the operator write gate** ([`crate::api::gate`]:
//! loopback peer, presenting the proxy's secret when a proxy is configured, no `Origin`,
//! the bearer token, `Content-Type: application/json`), because
//! what these routes list and change is the farm's store and the key to it. A server without
//! `--api-token-file` has them off, and one without `--object-store-secrets-dir` answers 404
//! to a caller who passed the gate. Every answer carries `Cache-Control: no-store`; every
//! error is `{"error": "...", "code": "..."}` with a closed `code`.
//!
//! **No route is fenced.** An old and a new frontend child serve these routes at once on one
//! state store: the registry's writes are compare-and-set on the generation row
//! ([`Registry`]), so a lost race is 409 `conflict`, never a lost write.
//!
//! **What the answers hold.** The secret key reaches nothing but its file: a body that does
//! not parse is refused with a sentence that names no value (a parser's own text quotes the
//! offending input), a probe's failure is the steps that passed and never a store's words,
//! and a registry or state failure is a code. A request is never logged.
//!
//! The registry's calls block, and so does a probe, so each route's work runs on the
//! blocking pool. The routes are a concrete [`ObjectStoreApi`] over a [`Registry`], a
//! [`Probe`] and an [`AckSource`]; the last two are traits because each has two
//! implementations (the real one and a test's).

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::header::{CACHE_CONTROL, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use kbf_types::object_store_api::{
    AuditList, AuditOutcome, BindingPut, BindingPutAnswer, BindingRole, ConnectionList,
    ConnectionName, ConnectionPut, ConnectionPutAnswer, Convergence, Deleted, ErrorBody,
    NodeConvergence, NodeStatus, RepointAnswer, RepointRequest, RoleFault, Status, StoreIdentity,
    Verified, VerifyAnswer, VerifyRequest,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::probe::{OriginFault, Probe, ProbeTarget, check_endpoint, check_origin};
use super::registry::{Registry, RegistryError};
use super::secrets::SecretError;
use crate::api::{gate, json};
use crate::proxy_auth::Caller;
use crate::token::ApiToken;

/// The path every route is under.
pub const BASE: &str = "/v1/object-stores";

/// The largest body a route takes. A connection is a few hundred bytes.
pub const BODY_MAX: usize = 16 * 1024;

/// How many audit rows `GET .../audit` returns without `limit`, and the most it returns.
const AUDIT_DEFAULT: NonZeroUsize = NonZeroUsize::new(50).unwrap();
const AUDIT_MAX: NonZeroUsize = NonZeroUsize::new(500).unwrap();

/// Longest access key id and secret access key, in characters.
const KEY_ID_MAX: usize = 256;
const SECRET_MAX: usize = 1024;

/// The longest signing region.
const REGION_MAX: usize = 64;

/// The custom method that probes a connection.
const VERIFY: &str = ":verify";

/// The custom method that moves the bound store of a connection.
const REPOINT: &str = ":repoint";

/// Where the nodes' acknowledgements of the registration are read.
pub trait AckSource: Send + Sync {
    /// Every storage node and what it reports, ids unique. The routes judge what the
    /// reports mean against the registry generation ([`Convergence::of`]).
    fn nodes(&self) -> Vec<NodeConvergence>;
}

/// What the routes need: the registry, the probe that checks a store before anything is
/// written, and where the nodes' reports come from.
pub struct ObjectStoreApi {
    registry: Arc<Registry>,
    probe: Arc<dyn Probe>,
    acks: Arc<dyn AckSource>,
}

impl ObjectStoreApi {
    /// The API over `registry`.
    #[must_use]
    pub fn new(registry: Arc<Registry>, probe: Arc<dyn Probe>, acks: Arc<dyn AckSource>) -> Self {
        Self {
            registry,
            probe,
            acks,
        }
    }
}

/// Why a call failed, as the status and closed code the operator gets. Never a secret, a
/// stored row or a store's own message.
#[derive(Debug)]
pub(crate) struct Failure {
    status: StatusCode,
    code: &'static str,
    why: String,
    verified: Option<Verified>,
}

impl Failure {
    fn new(status: StatusCode, code: &'static str, why: impl Into<String>) -> Self {
        Self {
            status,
            code,
            why: why.into(),
            verified: None,
        }
    }

    fn gate(status: StatusCode, why: String) -> Self {
        let code = match status {
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            _ => "forbidden",
        };
        Self::new(status, code, why)
    }

    fn bad_body(why: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_body", why)
    }

    fn bad_query() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "bad_query",
            "the query is limit=N, once, with N from 1 to 500",
        )
    }

    fn invalid(why: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_field", why)
    }

    fn probe(verified: Verified) -> Self {
        let mut failure = Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "probe_failed",
            "the store did not pass the probe: nothing was changed",
        );
        failure.verified = Some(verified);
        failure
    }

    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the object-store task failed",
        )
    }
}

impl From<RegistryError> for Failure {
    fn from(e: RegistryError) -> Self {
        use StatusCode as S;
        match e {
            RegistryError::GenerationMismatch { current } => Self::new(
                S::CONFLICT,
                "generation_mismatch",
                format!("the registry is at generation {current}; nothing was changed"),
            ),
            RegistryError::Conflict => Self::new(
                S::CONFLICT,
                "conflict",
                "the registry changed during the call; read it and try again",
            ),
            RegistryError::NoSuchConnection => {
                Self::new(S::NOT_FOUND, "no_such_connection", "no such connection")
            }
            RegistryError::NoSuchBinding => {
                Self::new(S::NOT_FOUND, "no_such_binding", "no such binding")
            }
            RegistryError::InUse => {
                Self::new(S::CONFLICT, "in_use", "a binding uses this connection")
            }
            RegistryError::NotBound => Self::new(
                S::CONFLICT,
                "not_bound",
                "the role cold is not bound to this connection: only the bound connection \
                 can be re-pointed",
            ),
            RegistryError::IdentityChange => Self::new(
                S::CONFLICT,
                "identity_change",
                "the endpoint, bucket and prefix of a bound role cannot change; \
                 credentials, region and the other settings can",
            ),
            RegistryError::Secret(SecretError::Io { .. }) => Self::new(
                S::INTERNAL_SERVER_ERROR,
                "secret_io",
                "the secret file could not be written or read",
            ),
            RegistryError::Secret(_) => Self::new(
                S::INTERNAL_SERVER_ERROR,
                "secret_missing",
                "the secret file of the connection is missing or not private (mode 0600); \
                 PUT the connection again to repair it",
            ),
            RegistryError::Corrupt(_) => Self::new(
                S::INTERNAL_SERVER_ERROR,
                "corrupt_rows",
                "a stored registration row is unreadable",
            ),
            RegistryError::Store(e) => {
                tracing::warn!(error = %e, "object-store routes: the state store failed");
                Self::new(
                    S::SERVICE_UNAVAILABLE,
                    "state_unavailable",
                    "the state store failed or refused the call",
                )
            }
        }
    }
}

impl From<OriginFault> for Failure {
    fn from(fault: OriginFault) -> Self {
        let code = match fault {
            OriginFault::Tls => "tls_unsupported",
            OriginFault::Endpoint => "invalid_endpoint",
            OriginFault::Bucket => "invalid_bucket",
            OriginFault::Prefix => "invalid_prefix",
        };
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, fault.to_string())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::warn!(code = self.code, "object-store routes: call failed");
        }
        let mut answer = json(
            self.status,
            &ErrorBody {
                error: self.why,
                code: self.code.to_owned(),
                verified: self.verified,
            },
        );
        if self.status == StatusCode::UNAUTHORIZED {
            answer
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        answer
    }
}

/// What the handlers share: the token the gate checks (`None`: writes are off) and the API
/// (`None`: the feature is off).
struct Routes {
    token: Option<ApiToken>,
    api: Option<Arc<ObjectStoreApi>>,
}

impl Routes {
    /// The API, if the caller passed the gate and the feature is on.
    fn admit(&self, peer: &Caller, headers: &HeaderMap) -> Result<Arc<ObjectStoreApi>, Failure> {
        gate(self.token.as_ref(), peer, headers)
            .map_err(|(status, why)| Failure::gate(status, why))?;
        self.api.clone().ok_or_else(|| {
            Failure::new(
                StatusCode::NOT_FOUND,
                "not_enabled",
                "object-store registration is not enabled on this frontend \
                 (--object-store-secrets-dir)",
            )
        })
    }
}

/// The routes. `token` is the operator API token the gate checks; `api` is what they call,
/// `None` when the frontend was started without `--object-store-secrets-dir`. Serve them
/// with `into_make_service_with_connect_info` behind [`crate::proxy_auth::gate_requests`],
/// as [`crate::api::router`] is: the handlers take the [`Caller`] that layer decided.
pub fn routes(token: Option<ApiToken>, api: Option<Arc<ObjectStoreApi>>) -> Router {
    Router::new()
        .route(BASE, get(status))
        .route("/v1/object-stores/audit", get(audit))
        .route("/v1/object-stores/connections", get(connections))
        .route(
            "/v1/object-stores/connections/{target}",
            get(connection)
                .put(put_connection)
                .delete(delete_connection)
                .post(connection_action),
        )
        .route(
            "/v1/object-stores/bindings/{role}",
            put(put_binding).delete(delete_binding),
        )
        .layer(DefaultBodyLimit::max(BODY_MAX))
        .layer(axum::middleware::map_response(no_store))
        .with_state(Arc::new(Routes { token, api }))
}

/// Marks every answer of these routes, errors and 405s included, as not for a cache.
async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Admits the call, runs `work` on the API, and answers.
async fn serve<T, Fut>(
    routes: &Routes,
    peer: &Caller,
    headers: &HeaderMap,
    work: impl FnOnce(Arc<ObjectStoreApi>) -> Fut,
) -> Response
where
    T: Serialize,
    Fut: Future<Output = Result<(StatusCode, T), Failure>>,
{
    let api = match routes.admit(peer, headers) {
        Ok(api) => api,
        Err(refusal) => return refusal.into_response(),
    };
    match work(api).await {
        Ok((status, value)) => json(status, &value),
        Err(failure) => failure.into_response(),
    }
}

/// Runs a call that blocks (every registry call and every probe does) off the async threads.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or_else(|_| Err(Failure::internal()))
}

fn connection_name(text: &str) -> Result<ConnectionName, Failure> {
    text.parse()
        .map_err(|e| Failure::new(StatusCode::BAD_REQUEST, "bad_name", format!("{e}")))
}

fn role_of(text: &str) -> Result<BindingRole, Failure> {
    text.parse().map_err(|fault: RoleFault| {
        Failure::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "role_not_supported",
            fault.to_string(),
        )
    })
}

/// The body of a request as `T`. The refusal names no value from the body: a parser's own
/// text quotes the offending input, and a body here holds a secret key.
fn parse<T: DeserializeOwned>(
    body: Result<Bytes, BytesRejection>,
    what: &str,
) -> Result<T, Failure> {
    let bytes = body_bytes(body)?;
    serde_json::from_slice(&bytes).map_err(|e| bad_json(&e, what))
}

/// As [`parse`], and an empty body is `T`'s default.
fn parse_or_default<T: DeserializeOwned + Default>(
    body: Result<Bytes, BytesRejection>,
    what: &str,
) -> Result<T, Failure> {
    let bytes = body_bytes(body)?;
    if bytes.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).map_err(|e| bad_json(&e, what))
}

fn body_bytes(body: Result<Bytes, BytesRejection>) -> Result<Bytes, Failure> {
    body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Failure::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!("the body is larger than {BODY_MAX} bytes"),
            )
        } else {
            Failure::bad_body("the body could not be read")
        }
    })
}

fn bad_json(e: &serde_json::Error, what: &str) -> Failure {
    use serde_json::error::Category;
    let kind = match e.classify() {
        Category::Syntax => "it is not JSON",
        Category::Eof => "it ends early",
        // `from_slice` reads memory: its errors are never `Io`.
        Category::Data | Category::Io => "a field is missing, unknown or of the wrong type",
    };
    Failure::bad_body(format!(
        "the body is not a {what}: {kind} (line {}, column {})",
        e.line(),
        e.column()
    ))
}

async fn status(State(state): State<Arc<Routes>>, peer: Caller, headers: HeaderMap) -> Response {
    serve(&state, &peer, &headers, |api| {
        blocking(move || api.status().map(|s| (StatusCode::OK, s)))
    })
    .await
}

async fn audit(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let limit = limit_of(query.as_deref())?;
        blocking(move || api.audit(limit).map(|a| (StatusCode::OK, a))).await
    })
    .await
}

async fn connections(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    headers: HeaderMap,
) -> Response {
    serve(&state, &peer, &headers, |api| {
        blocking(move || {
            Ok((
                StatusCode::OK,
                ConnectionList {
                    connections: api.registry.connections()?,
                },
            ))
        })
    })
    .await
}

async fn connection(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    Path(target): Path<String>,
    headers: HeaderMap,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let name = connection_name(&target)?;
        blocking(move || {
            let info = api.registry.connection(&name)?;
            info.map(|info| (StatusCode::OK, info))
                .ok_or_else(|| Failure::from(RegistryError::NoSuchConnection))
        })
        .await
    })
    .await
}

async fn put_connection(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    Path(target): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let name = connection_name(&target)?;
        let put: ConnectionPut = parse(body, "connection")?;
        blocking(move || api.put_connection(&name, &put)).await
    })
    .await
}

async fn delete_connection(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    Path(target): Path<String>,
    headers: HeaderMap,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let name = connection_name(&target)?;
        blocking(move || {
            let written = api.registry.delete_connection(&name)?;
            Ok((
                StatusCode::OK,
                Deleted {
                    generation: written.generation,
                },
            ))
        })
        .await
    })
    .await
}

/// `POST .../connections/{name}:verify` or `:repoint`: the router takes the whole segment, so
/// the verb is split off here.
async fn connection_action(
    state: State<Arc<Routes>>,
    peer: Caller,
    Path(target): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    match target.strip_suffix(REPOINT) {
        Some(name) => repoint(state, peer, name.to_owned(), headers, body).await,
        None => verify(state, peer, target, headers, body).await,
    }
}

async fn repoint(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    name: String,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let name = connection_name(&name)?;
        let request: RepointRequest = parse(body, "repoint request")?;
        blocking(move || api.repoint(&name, request)).await
    })
    .await
}

async fn verify(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    target: String,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let Some(name) = target.strip_suffix(VERIFY) else {
            return Err(Failure::new(
                StatusCode::NOT_FOUND,
                "unknown_action",
                "a POST to a connection is POST .../connections/{name}:verify or :repoint",
            ));
        };
        let name = connection_name(name)?;
        let request: VerifyRequest = parse_or_default(body, "verify request")?;
        blocking(move || api.verify(&name, request)).await
    })
    .await
}

async fn put_binding(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    Path(role): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let role = role_of(&role)?;
        let put: BindingPut = parse(body, "binding")?;
        blocking(move || api.put_binding(role, &put)).await
    })
    .await
}

async fn delete_binding(
    State(state): State<Arc<Routes>>,
    peer: Caller,
    Path(role): Path<String>,
    headers: HeaderMap,
) -> Response {
    serve(&state, &peer, &headers, |api| async move {
        let role = role_of(&role)?;
        blocking(move || api.delete_binding(role)).await
    })
    .await
}

/// The audit limit of a query: `limit=N` once, `N` from 1, capped at [`AUDIT_MAX`].
fn limit_of(query: Option<&str>) -> Result<NonZeroUsize, Failure> {
    let mut limit = None;
    for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key != "limit" || limit.is_some() {
            return Err(Failure::bad_query());
        }
        let parsed = value
            .parse::<usize>()
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or_else(Failure::bad_query)?;
        limit = Some(parsed);
    }
    Ok(limit.unwrap_or(AUDIT_DEFAULT).min(AUDIT_MAX))
}

/// A key id or secret: 1 to `max` visible ASCII characters (it is signed into a header).
fn is_key_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(|b| b.is_ascii_graphic())
}

/// A signing region: `[a-z0-9][a-z0-9-]*`, at most [`REGION_MAX`] characters.
fn is_region(text: &str) -> bool {
    text.len() <= REGION_MAX
        && text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_fields(put: &ConnectionPut) -> Result<(), Failure> {
    if !is_key_text(&put.access_key_id, KEY_ID_MAX) {
        return Err(Failure::invalid(format!(
            "access_key_id is 1 to {KEY_ID_MAX} visible ASCII characters"
        )));
    }
    if !is_key_text(put.secret_access_key.expose(), SECRET_MAX) {
        return Err(Failure::invalid(format!(
            "secret_access_key is 1 to {SECRET_MAX} visible ASCII characters"
        )));
    }
    if put
        .region
        .as_deref()
        .is_some_and(|region| !is_region(region))
    {
        return Err(Failure::invalid(format!(
            "region is 1 to {REGION_MAX} characters of a-z, 0-9 and -, starting with a letter or digit"
        )));
    }
    Ok(())
}

impl ObjectStoreApi {
    /// The bucket and prefix the `cold` binding gives `name`, if it is bound.
    fn cold_target(&self, name: &ConnectionName) -> Result<Option<(String, String)>, Failure> {
        Ok(self
            .registry
            .bindings()?
            .into_iter()
            .find(|b| {
                // Exhaustive on purpose: a second role must say here whose binding this is.
                let BindingRole::Cold = b.role;
                b.connection == *name
            })
            .map(|b| (b.bucket, b.prefix)))
    }

    /// Passes only a store that passes the probe; else 422 with the steps.
    fn probed(&self, target: &ProbeTarget<'_>) -> Result<Verified, Failure> {
        let verified = self.probe.probe(target);
        if verified.ok {
            Ok(verified)
        } else {
            Err(Failure::probe(verified))
        }
    }

    fn put_connection(
        &self,
        name: &ConnectionName,
        put: &ConnectionPut,
    ) -> Result<(StatusCode, ConnectionPutAnswer), Failure> {
        check_fields(put)?;
        check_endpoint(&put.endpoint)?;
        let mut verified = None;
        // Without a binding there is no bucket to probe: the connection is checked when
        // a role is bound to it, and by `:verify`.
        if put.verify != Some(false)
            && let Some((bucket, prefix)) = self.cold_target(name)?
        {
            verified = Some(self.probed(&ProbeTarget {
                endpoint: &put.endpoint,
                region: put.region.as_deref().unwrap_or_default(),
                access_key_id: &put.access_key_id,
                secret: &put.secret_access_key,
                bucket: &bucket,
                prefix: &prefix,
                conditional_put: put.conditional_put.unwrap_or(false),
                min_put_rate: put.min_put_rate,
            })?);
        }
        let written = self.registry.put_connection(name, put)?;
        let status = if written.outcome == AuditOutcome::Created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((
            status,
            ConnectionPutAnswer {
                name: name.clone(),
                generation: written.generation,
                changed: written.outcome != AuditOutcome::Unchanged,
                access_key_id: put.access_key_id.clone(),
                secret_set: true,
                verified,
            },
        ))
    }

    fn verify(
        &self,
        name: &ConnectionName,
        request: VerifyRequest,
    ) -> Result<(StatusCode, VerifyAnswer), Failure> {
        let info = self
            .registry
            .connection(name)?
            .ok_or(RegistryError::NoSuchConnection)?;
        let (bucket, prefix) = match request.bucket {
            Some(bucket) => (bucket, request.prefix.unwrap_or_default()),
            None => {
                let (bucket, prefix) = self.cold_target(name)?.ok_or_else(|| {
                    Failure::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "no_bucket",
                        "no bucket to probe: bind the role cold to this connection, or send {\"bucket\": ...}",
                    )
                })?;
                (bucket, request.prefix.unwrap_or(prefix))
            }
        };
        check_origin(&info.endpoint, &bucket, &prefix)?;
        let secret = self.registry.secret(name)?;
        let verified = self.probed(&ProbeTarget {
            endpoint: &info.endpoint,
            region: &info.region,
            access_key_id: &info.access_key_id,
            secret: &secret,
            bucket: &bucket,
            prefix: &prefix,
            conditional_put: info.conditional_put,
            min_put_rate: info.min_put_rate,
        })?;
        Ok((
            StatusCode::OK,
            VerifyAnswer {
                name: name.clone(),
                verified,
            },
        ))
    }

    /// Moves the store the role `cold` names to `request`'s endpoint, bucket and prefix
    /// (the binding's own where the request gives none), once the connection's key has
    /// passed the probe of the new store. There is no way to skip the probe.
    fn repoint(
        &self,
        name: &ConnectionName,
        request: RepointRequest,
    ) -> Result<(StatusCode, RepointAnswer), Failure> {
        let info = self
            .registry
            .connection(name)?
            .ok_or(RegistryError::NoSuchConnection)?;
        let (bucket, prefix) = self.cold_target(name)?.ok_or(RegistryError::NotBound)?;
        let to = StoreIdentity {
            endpoint: request.endpoint,
            bucket: request.bucket.unwrap_or(bucket),
            prefix: request.prefix.unwrap_or(prefix),
        };
        check_origin(&to.endpoint, &to.bucket, &to.prefix)?;
        let secret = self.registry.secret(name)?;
        let verified = self.probed(&ProbeTarget {
            endpoint: &to.endpoint,
            region: &info.region,
            access_key_id: &info.access_key_id,
            secret: &secret,
            bucket: &to.bucket,
            prefix: &to.prefix,
            conditional_put: info.conditional_put,
            min_put_rate: info.min_put_rate,
        })?;
        let (written, from) = self
            .registry
            .repoint(name, &to, request.expect_generation)?;
        Ok((
            StatusCode::OK,
            RepointAnswer {
                name: name.clone(),
                generation: written.generation,
                changed: written.outcome != AuditOutcome::Unchanged,
                from,
                to,
                verified,
            },
        ))
    }

    fn put_binding(
        &self,
        role: BindingRole,
        put: &BindingPut,
    ) -> Result<(StatusCode, BindingPutAnswer), Failure> {
        let connection = self.registry.connection(&put.connection)?.ok_or_else(|| {
            Failure::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_connection",
                "the binding names a connection that does not exist",
            )
        })?;
        check_origin(&connection.endpoint, &put.bucket, &put.prefix)?;
        let mut verified = None;
        if put.verify != Some(false) {
            let secret = self.registry.secret(&put.connection)?;
            verified = Some(self.probed(&ProbeTarget {
                endpoint: &connection.endpoint,
                region: &connection.region,
                access_key_id: &connection.access_key_id,
                secret: &secret,
                bucket: &put.bucket,
                prefix: &put.prefix,
                conditional_put: connection.conditional_put,
                min_put_rate: connection.min_put_rate,
            })?);
        }
        let written = self.registry.put_binding(role, put)?;
        let status = if written.outcome == AuditOutcome::Created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((
            status,
            BindingPutAnswer {
                role,
                generation: written.generation,
                changed: written.outcome != AuditOutcome::Unchanged,
                verified,
            },
        ))
    }

    /// Unbinds `role`, unless a node has applied the binding: it runs on that store, and
    /// there is no way to take a store back from a running node.
    fn delete_binding(&self, role: BindingRole) -> Result<(StatusCode, Deleted), Failure> {
        let binding = self
            .registry
            .bindings()?
            .into_iter()
            .find(|b| b.role == role)
            .ok_or(RegistryError::NoSuchBinding)?;
        let applied = self.acks.nodes().iter().any(|node| {
            node.status == NodeStatus::Ok
                && node
                    .applied_generation
                    .is_some_and(|generation| generation >= binding.updated_generation)
        });
        if applied {
            return Err(RegistryError::InUse.into());
        }
        let written = self.registry.delete_binding(role)?;
        Ok((
            StatusCode::OK,
            Deleted {
                generation: written.generation,
            },
        ))
    }

    fn status(&self) -> Result<Status, Failure> {
        let generation = self.registry.generation()?;
        Ok(Status {
            generation,
            connections: self.registry.connections()?,
            bindings: self.registry.bindings()?,
            convergence: Convergence::of(generation, self.acks.nodes()),
        })
    }

    fn audit(&self, limit: NonZeroUsize) -> Result<AuditList, Failure> {
        Ok(AuditList {
            entries: self.registry.audit(limit)?,
        })
    }
}
