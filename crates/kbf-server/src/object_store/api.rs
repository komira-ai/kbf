//! The control API for the object-store registration: `/v1/object-stores/...`.
//!
//! STUB (red first): every route answers 501 and nothing is checked.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use kbf_types::object_store_api::NodeConvergence;

use super::probe::Probe;
use super::registry::Registry;
use crate::token::ApiToken;

/// The path every route is under.
pub const BASE: &str = "/v1/object-stores";

/// Where the nodes' acknowledgements of the registration are read.
pub trait AckSource: Send + Sync {
    /// Every storage node and what it reports.
    fn nodes(&self) -> Vec<NodeConvergence>;
}

/// An [`AckSource`] that knows no node.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAcks;

impl AckSource for NoAcks {
    fn nodes(&self) -> Vec<NodeConvergence> {
        Vec::new()
    }
}

/// What the routes need.
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

impl std::fmt::Debug for ObjectStoreApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreApi").finish_non_exhaustive()
    }
}

/// The routes.
pub fn routes(token: Option<ApiToken>, api: Option<Arc<ObjectStoreApi>>) -> Router {
    let _ = (token, api);
    Router::new()
        .route(BASE, any(not_built))
        .route("/v1/object-stores/audit", any(not_built))
        .route("/v1/object-stores/connections", any(not_built))
        .route("/v1/object-stores/connections/{target}", any(not_built))
        .route("/v1/object-stores/bindings/{role}", any(not_built))
}

async fn not_built() -> Response {
    StatusCode::NOT_IMPLEMENTED.into_response()
}
