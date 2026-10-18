//! The remote operator listener: two routes for CI, off unless `--remote-operator-listen`
//! is given (`docs/design/frontend-on-k8s.md` section 3).
//!
//! - `POST /v1/update/desired` with `{"commit": "<40 or 64 lowercase hex>"}`: offers that
//!   release to the node update ([`crate::update::Rows::offer`]); the one already desired
//!   changes nothing.
//! - `POST /v1/update/frontend` with `{"image_digest": "sha256:<64 hex>"}`: the frontend's
//!   own image. Nothing applies it yet, so it answers 501 (the self-update is a later change).
//!
//! The listener serves nothing else: no read, no `admin` write, and `api.rs` and its
//! loopback-only `gate()` are untouched. A request passes, in order: a body limit (413), no
//! `Origin` header (403), `Authorization: Bearer <GitHub OIDC token>` that the
//! [`Verifier`](crate::oidc::Verifier) accepts (401; 503 when the issuer's keys cannot be
//! had and none are held), `Content-Type: application/json` (415), and a body of exactly the
//! fields above (400): a `strategy` or any other field is refused, so CI cannot pick how a
//! rollout runs. The token is spent by a verified request even if the body is then refused.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use crate::api::{error, is_json, json};
use crate::oidc::{Refusal, Verified, Verifier};

/// The path of `POST /v1/update/desired`.
pub const UPDATE_DESIRED: &str = "/v1/update/desired";

/// The path of `POST /v1/update/frontend`.
pub const UPDATE_FRONTEND: &str = "/v1/update/frontend";

/// The most a request body may be.
pub const MAX_BODY_BYTES: usize = 4096;

/// What a verified request is allowed to ask for.
pub trait RolloutSink: Send + Sync + 'static {
    /// Offers `commit` as the desired release. Blocking.
    ///
    /// # Errors
    /// [`SinkError`].
    fn offer_desired(&self, commit: &str, who: &Verified) -> Result<Outcome, SinkError>;

    /// Sets the frontend's image to `image` (`repository@sha256:...`). Blocking.
    ///
    /// # Errors
    /// [`SinkError`].
    fn update_frontend(&self, image: &str, who: &Verified) -> Result<Outcome, SinkError>;
}

/// What a sink did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The request changed the desired state.
    Applied,
    /// The state already was what the request names.
    Unchanged,
}

/// Why a sink did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkError {
    /// This server cannot do that (501).
    NotConfigured(String),
    /// It could not, now (503).
    Failed(String),
}

struct Remote {
    verifier: Arc<Verifier>,
    sink: Arc<dyn RolloutSink>,
}

/// The listener's routes. Serve them with `into_make_service_with_connect_info` or without:
/// the peer address decides nothing here.
pub fn router(verifier: Arc<Verifier>, sink: Arc<dyn RolloutSink>) -> Router {
    Router::new()
        .route(UPDATE_DESIRED, post(desired))
        .route(UPDATE_FRONTEND, post(frontend))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(Arc::new(Remote { verifier, sink }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredBody {
    commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontendBody {
    image_digest: String,
}

/// The token, verified, and the headers' other gates.
async fn admit(remote: &Remote, headers: &HeaderMap) -> Result<Verified, Response> {
    if headers.contains_key(ORIGIN) {
        return Err(error(
            StatusCode::FORBIDDEN,
            "a request from a browser page (with an Origin header) is refused",
        ));
    }
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "Authorization: Bearer <an OIDC token> is required",
        ));
    };
    let who = remote.verifier.verify(token).await.map_err(|refusal| {
        tracing::warn!(%refusal, "remote operator: token refused");
        if refusal == Refusal::KeysUnavailable {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the issuer's keys are unavailable",
            )
        } else {
            error(
                StatusCode::UNAUTHORIZED,
                &format!("token refused: {}", refusal.name()),
            )
        }
    })?;
    let json_body = headers
        .get(CONTENT_TYPE)
        .is_some_and(|v| is_json(v.as_bytes()));
    if !json_body {
        return Err(error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send Content-Type: application/json",
        ));
    }
    Ok(who)
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn answer(outcome: Result<Result<Outcome, SinkError>, tokio::task::JoinError>) -> Response {
    match outcome {
        Ok(Ok(Outcome::Applied)) => json(StatusCode::OK, &serde_json::json!({"status": "applied"})),
        Ok(Ok(Outcome::Unchanged)) => {
            json(StatusCode::OK, &serde_json::json!({"status": "unchanged"}))
        }
        Ok(Err(SinkError::NotConfigured(why))) => error(StatusCode::NOT_IMPLEMENTED, &why),
        Ok(Err(SinkError::Failed(why))) => error(StatusCode::SERVICE_UNAVAILABLE, &why),
        Err(e) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("rollout task: {e}"),
        ),
    }
}

async fn desired(State(remote): State<Arc<Remote>>, headers: HeaderMap, body: Bytes) -> Response {
    let who = match admit(&remote, &headers).await {
        Ok(who) => who,
        Err(refused) => return refused,
    };
    let want: DesiredBody = match serde_json::from_slice(&body) {
        Ok(want) => want,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("the body is {{\"commit\": \"<hex>\"}} and nothing else: {e}"),
            );
        }
    };
    if !is_hex(&want.commit, 40) && !is_hex(&want.commit, 64) {
        return error(
            StatusCode::BAD_REQUEST,
            "commit is 40 or 64 lowercase hex digits",
        );
    }
    tracing::info!(jti = %who.jti, workflow = %who.job_workflow_ref, commit = %want.commit,
        "remote operator: desired release offered");
    let sink = Arc::clone(&remote.sink);
    answer(tokio::task::spawn_blocking(move || sink.offer_desired(&want.commit, &who)).await)
}

async fn frontend(State(remote): State<Arc<Remote>>, headers: HeaderMap, body: Bytes) -> Response {
    let who = match admit(&remote, &headers).await {
        Ok(who) => who,
        Err(refused) => return refused,
    };
    let want: FrontendBody = match serde_json::from_slice(&body) {
        Ok(want) => want,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!(
                    "the body is {{\"image_digest\": \"sha256:<hex>\"}} and nothing else: {e}"
                ),
            );
        }
    };
    let Some(digest) = want
        .image_digest
        .strip_prefix("sha256:")
        .filter(|d| is_hex(d, 64))
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "image_digest is sha256: and 64 lowercase hex digits",
        );
    };
    let Some(repository) = remote.verifier.config().frontend_image_repository.clone() else {
        return error(
            StatusCode::NOT_IMPLEMENTED,
            "frontend_image_repository is not configured",
        );
    };
    let image = format!("{repository}@sha256:{digest}");
    tracing::info!(jti = %who.jti, workflow = %who.job_workflow_ref, %image,
        "remote operator: frontend image offered");
    let sink = Arc::clone(&remote.sink);
    answer(tokio::task::spawn_blocking(move || sink.update_frontend(&image, &who)).await)
}

/// The sink of a frontend with a state store: offers go to the update rows; the frontend's
/// own image is not applied by anything yet.
pub struct StoreSink {
    rows: crate::update::Rows,
    now: Arc<dyn Fn() -> kbf_types::FarmTime + Send + Sync>,
}

impl StoreSink {
    /// A sink over `store`.
    #[must_use]
    pub fn new(
        store: Arc<dyn kbf_state::Store>,
        now: Arc<dyn Fn() -> kbf_types::FarmTime + Send + Sync>,
    ) -> Self {
        Self {
            rows: crate::update::Rows::new(store),
            now,
        }
    }
}

impl RolloutSink for StoreSink {
    fn offer_desired(&self, commit: &str, _who: &Verified) -> Result<Outcome, SinkError> {
        let release = crate::update::Release::new(commit);
        let before = self
            .rows
            .desired()
            .map_err(|e| SinkError::Failed(e.to_string()))?;
        let now = (self.now)();
        let after = self
            .rows
            .offer(&release, now)
            .map_err(|e| SinkError::Failed(e.to_string()))?;
        Ok(
            if before
                .value
                .as_ref()
                .is_some_and(|d| d.generation == after.generation)
            {
                Outcome::Unchanged
            } else {
                Outcome::Applied
            },
        )
    }

    fn update_frontend(&self, _image: &str, _who: &Verified) -> Result<Outcome, SinkError> {
        Err(SinkError::NotConfigured(
            "this frontend cannot apply its own image yet".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests;
