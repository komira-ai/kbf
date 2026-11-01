//! The operator's token and node API (`docs/api.md`):
//!
//! - `POST /v1/enroll/tokens`: makes a token ([`TokenCreate`] in, [`TokenCreated`] out).
//!   The answer is the only place the secret and the full token string
//!   (`kbft1.<id>.<secret>.<pin>`) are ever shown.
//! - `GET /v1/enroll/tokens`: every token ([`TokenList`]): no secret, no hash, no string.
//! - `DELETE /v1/enroll/tokens/{id}`: revokes a token ([`TokenRevoked`]); nodes it enrolled
//!   stay enrolled.
//! - `GET /v1/enroll/nodes`: every node in the registry ([`NodeList`]).
//! - `POST /v1/enroll/nodes/{id}/revoke`: revokes a node and writes its deny rows
//!   ([`NodeRevoked`]).
//! - `POST /v1/enroll/nodes/{id}:adopt`: records a node provisioned before enrollment, with
//!   no certificate ([`AdoptRequest`] in, the node as the listing shows it out).
//!
//! **Every route, a GET included, passes the operator write gate** ([`crate::api::gate`]:
//! loopback peer, no `Origin`, the bearer token, `Content-Type: application/json`), because
//! what these routes list is the farm's door. A server without `--api-token-file` has them
//! off, and one without enrollment answers 404 to a caller who passed the gate.
//!
//! The routes call `kbf-enroll-registry` over the frontend's state store handle, which the
//! caller passes already fenced: a frontend that has lost the writer role writes nothing and
//! answers 409 (`NotSigner` on the enroll wire). The store's calls block, so each runs on
//! the blocking pool. The CA pin comes from a [`PinSource`] and the bytes of a token from a
//! [`Random`]; nothing here reads the CA or draws its own randomness.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use kbf_enroll_registry::{
    NodeState, RegistryError, TokenSpec, adopt_node, canonical_addr, create_token, list_nodes,
    list_tokens, revoke_node, revoke_token,
};
use kbf_state::{Store, StoreError};
use kbf_types::enroll_api::{
    NodeInfo, NodeList, NodeRevoked, TokenCreate, TokenCreated, TokenInfo, TokenList, TokenRevoked,
};
use kbf_types::{HostRole, NodeId, RoleSet, Token, TokenId, TokenSecret};
use serde::{Deserialize, Serialize};

use super::service::{advertise_refused, node_id_refused};
use super::{PinSource, Random};
use crate::api::{error, gate, json};
use crate::members::Members;
use crate::proxy_auth::Caller;
use crate::token::ApiToken;

/// The path of `POST` (create) and `GET` (list) tokens.
pub const TOKENS: &str = "/v1/enroll/tokens";

/// The path of `GET /v1/enroll/nodes`.
pub const NODES: &str = "/v1/enroll/nodes";

/// What the registry records as the writer of a token or a revocation made here.
const OPERATOR: &str = "operator-api";

/// What the routes need: the frontend's state store handle (fenced, so every write checks
/// that this frontend still holds the writer role), the source of the CA pin and the
/// source of random bytes.
pub struct EnrollApi {
    store: Arc<dyn Store>,
    pins: Arc<dyn PinSource>,
    random: Arc<dyn Random>,
    /// The membership, refreshed at once after a node revoke; `None` leaves that to the
    /// sweeper's next beat.
    members: Option<Arc<Members>>,
    /// The names this frontend serves: no node adopted here may be called one, or advertise
    /// one, as no enrollment may.
    frontend_names: Vec<String>,
}

impl EnrollApi {
    /// The API over `store`, which must be the frontend's fenced handle.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, pins: Arc<dyn PinSource>, random: Arc<dyn Random>) -> Self {
        Self {
            store,
            pins,
            random,
            members: None,
            frontend_names: Vec::new(),
        }
    }

    /// The API refusing, for an adopted node, the id and the advertise host that enrollment
    /// refuses for the names this frontend serves.
    #[must_use]
    pub fn with_frontend_names(mut self, names: &[String]) -> Self {
        self.frontend_names = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        self
    }

    /// The API refreshing `members` right after a node revoke, as `POST /v1/denied` does,
    /// so the map storage nodes follow carries the new deny rows at once and not at the
    /// next sweep.
    #[must_use]
    pub fn with_members(mut self, members: Option<Arc<Members>>) -> Self {
        self.members = members;
        self
    }
}

impl std::fmt::Debug for EnrollApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollApi").finish_non_exhaustive()
    }
}

/// Why a call failed, as the status and text the operator gets. Never a secret, a stored
/// row or the store's own message.
#[derive(Debug)]
pub(crate) struct Failure(StatusCode, String);

impl Failure {
    fn new(code: StatusCode, why: impl Into<String>) -> Self {
        Self(code, why.into())
    }
}

impl From<RegistryError> for Failure {
    fn from(e: RegistryError) -> Self {
        match e {
            RegistryError::Invalid(why) => Self::new(StatusCode::BAD_REQUEST, why),
            RegistryError::NotFound => Self::new(StatusCode::NOT_FOUND, "not found"),
            RegistryError::AlreadyExists(why) => Self::new(StatusCode::CONFLICT, why),
            RegistryError::Corrupt(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "a stored enrollment row is unreadable",
            ),
            RegistryError::Contended => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "other writers kept winning; try again",
            ),
            RegistryError::Store(StoreError::Fenced) => Self::new(
                StatusCode::CONFLICT,
                "this frontend is not the signer: it no longer holds the writer role; \
                 call the current frontend",
            ),
            RegistryError::Store(StoreError::Failed(_)) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "the state store failed")
            }
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        error(self.0, &self.1)
    }
}

/// What the handlers share: the token the gate checks (`None`: writes are off) and the
/// enrollment (`None`: this server has none).
pub(crate) struct EnrollState {
    token: Option<ApiToken>,
    enroll: Option<Arc<EnrollApi>>,
}

impl EnrollState {
    /// The enrollment, if the caller passed the gate and there is one.
    fn admit(&self, peer: &Caller, headers: &HeaderMap) -> Result<Arc<EnrollApi>, Box<Response>> {
        gate(self.token.as_ref(), peer, headers)
            .map_err(|(code, why)| Box::new(error(code, &why)))?;
        self.enroll.clone().ok_or_else(|| {
            Box::new(error(
                StatusCode::NOT_FOUND,
                "enrollment is not enabled on this frontend",
            ))
        })
    }
}

/// The six routes. `token` is the operator API token the gate checks; `enroll` is what
/// they call, `None` when this server has no enrollment. Serve them with
/// `into_make_service_with_connect_info`, as [`crate::api::router`] is.
pub fn routes(token: Option<ApiToken>, enroll: Option<Arc<EnrollApi>>) -> Router {
    Router::new()
        .route(TOKENS, post(create).get(list))
        .route("/v1/enroll/tokens/{id}", delete(revoke_token_route))
        .route(NODES, get(nodes))
        .route("/v1/enroll/nodes/{id}/revoke", post(revoke_node_route))
        // The router takes one parameter to a segment and no text after it, so the segment is
        // `{id}:adopt` whole and the handler splits it.
        .route("/v1/enroll/nodes/{id_and_verb}", post(adopt_node_route))
        .with_state(Arc::new(EnrollState { token, enroll }))
}

/// `value` as a JSON body that no cache keeps.
fn reply(code: StatusCode, value: &impl Serialize) -> Response {
    let mut answer = json(code, value);
    answer
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    answer
}

fn answer<T: Serialize>(result: Result<T, Failure>) -> Response {
    match result {
        Ok(value) => reply(StatusCode::OK, &value),
        Err(failure) => failure.into_response(),
    }
}

/// Runs a call that blocks (every store call does) off the async threads.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(call).await.unwrap_or_else(|_| {
        Err(Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the enrollment task failed",
        ))
    })
}

pub(crate) async fn create(
    State(state): State<Arc<EnrollState>>,
    peer: Caller,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let api = match state.admit(&peer, &headers) {
        Ok(api) => api,
        Err(refusal) => return *refusal,
    };
    let request: TokenCreate = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            let why = format!("a token request is JSON like {{\"roles\": [\"daemon\"]}}: {e}");
            return error(StatusCode::BAD_REQUEST, &why);
        }
    };
    answer(blocking(move || make_token(&api, request)).await)
}

pub(crate) async fn list(
    State(state): State<Arc<EnrollState>>,
    peer: Caller,
    headers: HeaderMap,
) -> Response {
    let api = match state.admit(&peer, &headers) {
        Ok(api) => api,
        Err(refusal) => return *refusal,
    };
    answer(blocking(move || token_list(&api)).await)
}

pub(crate) async fn revoke_token_route(
    State(state): State<Arc<EnrollState>>,
    peer: Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let api = match state.admit(&peer, &headers) {
        Ok(api) => api,
        Err(refusal) => return *refusal,
    };
    answer(blocking(move || revoke_token_by_id(&api, &id)).await)
}

pub(crate) async fn nodes(
    State(state): State<Arc<EnrollState>>,
    peer: Caller,
    headers: HeaderMap,
) -> Response {
    let api = match state.admit(&peer, &headers) {
        Ok(api) => api,
        Err(refusal) => return *refusal,
    };
    answer(blocking(move || node_list(&api)).await)
}

/// The body a node revoke may carry: none, or `{}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoBody {}

pub(crate) async fn revoke_node_route(
    State(state): State<Arc<EnrollState>>,
    peer: Caller,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let api = match state.admit(&peer, &headers) {
        Ok(api) => api,
        Err(refusal) => return *refusal,
    };
    if !body.is_empty()
        && let Err(e) = serde_json::from_slice::<NoBody>(&body)
    {
        let why = format!("a node revoke takes no body, or {{}}: {e}");
        return error(StatusCode::BAD_REQUEST, &why);
    }
    answer(blocking(move || revoke_node_by_id(&api, &id)).await)
}

/// The body of an adopt: the roles the node holds and, for a storage node, the `host:port`
/// it advertises.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdoptRequest {
    roles: Vec<String>,
    #[serde(default)]
    advertise: String,
}

pub(crate) async fn adopt_node_route(
    State(state): State<Arc<EnrollState>>,
    peer: Caller,
    Path(id_and_verb): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let api = match state.admit(&peer, &headers) {
        Ok(api) => api,
        Err(refusal) => return *refusal,
    };
    let Some(id) = id_and_verb.strip_suffix(":adopt").map(str::to_owned) else {
        return error(StatusCode::NOT_FOUND, "no such route");
    };
    let request: AdoptRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            let why = format!(
                "an adopt is JSON like {{\"roles\": [\"storage\"], \"advertise\": \"host:7000\"}}: {e}"
            );
            return error(StatusCode::BAD_REQUEST, &why);
        }
    };
    answer(blocking(move || adopt_node_by_id(&api, &id, &request)).await)
}

fn bad(why: impl Into<String>) -> Failure {
    Failure::new(StatusCode::BAD_REQUEST, why)
}

fn token_dto(info: &kbf_enroll_registry::TokenInfo) -> TokenInfo {
    TokenInfo {
        id: info.id.to_string(),
        roles: info.roles.iter().map(|r| r.name().to_owned()).collect(),
        host_role: info.host_role.as_ref().map(|role| role.name().to_owned()),
        max_uses: info.max_uses,
        uses: info.uses,
        expires_at: info.expires_at,
        created_at: info.created_at,
        revoked_at: info.revoked_at,
        node_id: info.node_id.clone(),
        node_prefix: info.node_prefix.clone(),
        names: info.names.clone(),
        replace: info.replace,
        label: info.label.clone(),
        created_by: info.created_by.clone(),
    }
}

fn node_dto(info: &kbf_enroll_registry::NodeInfo) -> NodeInfo {
    NodeInfo {
        id: info.id.to_string(),
        roles: info.roles.iter().map(|r| r.name().to_owned()).collect(),
        state: match info.state {
            NodeState::Active => "active",
            NodeState::Revoked => "revoked",
        }
        .to_owned(),
        advertise: info.advertise.clone(),
        zone: info.zone.clone(),
        token_id: info.token_id.clone(),
        token_label: info.token_label.clone(),
        created_at: info.created_at,
        last_renewed_at: info.last_renewed_at,
        not_after: info.not_after,
        revoked_at: info.revoked_at,
    }
}

/// Makes a token. The pin and the random bytes come first, so a token whose string cannot
/// be built is never stored; the store keeps a hash of the secret, and the secret leaves
/// this function once, in the answer.
fn make_token(api: &EnrollApi, request: TokenCreate) -> Result<TokenCreated, Failure> {
    let roles = RoleSet::from_names(request.roles.iter().map(String::as_str))
        .map_err(|e| bad(e.to_string()))?;
    let host_role = request
        .host_role
        .as_deref()
        .map(HostRole::parse_named)
        .transpose()
        .map_err(|e| bad(format!("host_role: {e}")))?;
    let node_id = request
        .node_id
        .as_deref()
        .map(str::parse::<NodeId>)
        .transpose()
        .map_err(|e| bad(format!("node_id: {e}")))?;
    let spec = TokenSpec {
        roles,
        host_role,
        max_uses: request.max_uses,
        ttl: request.expires_in.map(Duration::from_secs),
        node_id,
        node_prefix: request.node_prefix,
        names: request.names,
        replace: request.replace,
        label: request.label,
        created_by: OPERATOR.to_owned(),
    };
    let pin = api.pins.pin().map_err(|e| {
        Failure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("no CA pin for the token: {e}"),
        )
    })?;
    let unpredictable = || {
        Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the random source failed; no token was made",
        )
    };
    let (mut id_bytes, mut secret_bytes) = ([0u8; 8], [0u8; 32]);
    api.random
        .fill(&mut id_bytes)
        .map_err(|_| unpredictable())?;
    api.random
        .fill(&mut secret_bytes)
        .map_err(|_| unpredictable())?;
    let id = TokenId::from_bits(u64::from_be_bytes(id_bytes));
    let secret = TokenSecret::from_bytes(secret_bytes);
    let info = create_token(&*api.store, &id, &secret, &spec)?;
    let line = Token::new(id, secret, pin).to_line();
    let secret = line.split('.').nth(2).unwrap_or_default().to_owned();
    Ok(TokenCreated {
        token: line,
        secret,
        info: token_dto(&info),
    })
}

fn token_list(api: &EnrollApi) -> Result<TokenList, Failure> {
    let tokens = list_tokens(&*api.store)?;
    Ok(TokenList {
        tokens: tokens.iter().map(token_dto).collect(),
    })
}

fn revoke_token_by_id(api: &EnrollApi, id: &str) -> Result<TokenRevoked, Failure> {
    let parsed = TokenId::parse(id)
        .map_err(|_| Failure::new(StatusCode::NOT_FOUND, format!("no token {id:?}")))?;
    revoke_token(&*api.store, &parsed)?;
    Ok(TokenRevoked {
        id: parsed.to_string(),
    })
}

fn node_list(api: &EnrollApi) -> Result<NodeList, Failure> {
    let nodes = list_nodes(&*api.store)?;
    Ok(NodeList {
        nodes: nodes.iter().map(node_dto).collect(),
    })
}

fn adopt_node_by_id(
    api: &EnrollApi,
    id: &str,
    request: &AdoptRequest,
) -> Result<NodeInfo, Failure> {
    let parsed = id
        .parse::<NodeId>()
        .map_err(|e| bad(format!("node id {id:?}: {e}")))?;
    let roles = RoleSet::from_names(request.roles.iter().map(String::as_str))
        .map_err(|e| bad(e.to_string()))?;
    if node_id_refused(parsed.as_str(), &api.frontend_names) {
        return Err(bad("that node id is a name no node may hold"));
    }
    let advertise = canonical_addr(&request.advertise);
    if let Some((host, _)) = advertise.rsplit_once(':')
        && advertise_refused(host, &api.frontend_names)
    {
        return Err(bad("that advertise host is not one a node may advertise"));
    }
    let info = adopt_node(&*api.store, &parsed, roles, &request.advertise)?;
    Ok(node_dto(&info))
}

fn revoke_node_by_id(api: &EnrollApi, id: &str) -> Result<NodeRevoked, Failure> {
    let parsed = id
        .parse::<NodeId>()
        .map_err(|_| Failure::new(StatusCode::NOT_FOUND, format!("no node {id:?}")))?;
    let denied = revoke_node(&*api.store, &parsed, OPERATOR)?;
    // The rows are written and every check of the worker listener and of registration reads
    // them; the map follows at once. A failure here changes no one's admission, and the
    // sweeper's next beat does the same, so it is logged, not returned.
    if let Some(members) = &api.members
        && let Err(error) = members.refresh()
    {
        tracing::warn!(%error, node = %parsed, "refreshing the map after a node revoke failed; the next beat does it");
    }
    Ok(NodeRevoked {
        id: parsed.to_string(),
        denied: denied.iter().map(ToString::to_string).collect(),
    })
}

#[cfg(test)]
mod tests;
