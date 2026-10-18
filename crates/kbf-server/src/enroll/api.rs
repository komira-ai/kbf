//! STUB (red): handlers answer 501.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use kbf_state::Store;

use kbf_enroll_registry::RegistryError;

use super::{PinSource, Random};
use crate::token::ApiToken;

pub const TOKENS: &str = "/v1/enroll/tokens";
pub const NODES: &str = "/v1/enroll/nodes";

pub struct EnrollApi {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) pins: Arc<dyn PinSource>,
    pub(crate) random: Arc<dyn Random>,
}

impl EnrollApi {
    #[must_use]
    pub fn new(store: Arc<dyn Store>, pins: Arc<dyn PinSource>, random: Arc<dyn Random>) -> Self {
        Self {
            store,
            pins,
            random,
        }
    }
}

impl std::fmt::Debug for EnrollApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollApi").finish_non_exhaustive()
    }
}

pub(crate) struct Failure(StatusCode, String);

impl From<RegistryError> for Failure {
    fn from(_: RegistryError) -> Self {
        Self(StatusCode::NOT_IMPLEMENTED, String::new())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        self.0.into_response()
    }
}

pub(crate) struct EnrollState {
    pub(crate) token: Option<ApiToken>,
    pub(crate) enroll: Option<Arc<EnrollApi>>,
}

pub fn routes(token: Option<ApiToken>, enroll: Option<Arc<EnrollApi>>) -> Router {
    Router::new()
        .route(TOKENS, post(create).get(list))
        .route("/v1/enroll/tokens/{id}", delete(revoke_token))
        .route(NODES, get(nodes))
        .route("/v1/enroll/nodes/{id}/revoke", post(revoke_node))
        .with_state(Arc::new(EnrollState { token, enroll }))
}

pub(crate) async fn create(
    State(_): State<Arc<EnrollState>>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    _: HeaderMap,
    _: Bytes,
) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

pub(crate) async fn list(
    State(_): State<Arc<EnrollState>>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    _: HeaderMap,
) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

pub(crate) async fn revoke_token(
    State(_): State<Arc<EnrollState>>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    Path(_): Path<String>,
    _: HeaderMap,
) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

pub(crate) async fn nodes(
    State(_): State<Arc<EnrollState>>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    _: HeaderMap,
) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

pub(crate) async fn revoke_node(
    State(_): State<Arc<EnrollState>>,
    ConnectInfo(_): ConnectInfo<SocketAddr>,
    Path(_): Path<String>,
    _: HeaderMap,
    _: Bytes,
) -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[cfg(test)]
mod tests;
