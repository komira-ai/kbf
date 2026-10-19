//! The admin calls, over HTTP on a loopback port.
//!
//! | call | does |
//! |---|---|
//! | `GET /v1/admin/role` | the [`RoleView`] |
//! | `POST /v1/admin/quiesce` | stop scheduling, assigning and requeueing; flush |
//! | `POST /v1/admin/resume` | undo a quiesce, while the store is at this process's generation |
//! | `POST /v1/admin/takeover` `{"expect": N}` | a candidate takes generation N |
//! | `POST /v1/admin/stand-down` | after a takeover elsewhere |
//!
//! Each answers `200` with the [`RoleView`] after the call, `409` with an `error` and the
//! view when the call does not apply (the role, or the generation), `400` for a body that
//! is not what the call takes and `500` when a takeover took the generation and could not
//! start.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use super::control::{Control, Refusal, RoleView};

/// The routes.
pub(crate) fn router(control: Arc<Control>) -> Router {
    Router::new()
        .route("/v1/admin/role", get(role))
        .route("/v1/admin/quiesce", post(quiesce))
        .route("/v1/admin/resume", post(resume))
        .route("/v1/admin/takeover", post(takeover))
        .route("/v1/admin/stand-down", post(stand_down))
        .with_state(control)
}

fn json(code: StatusCode, value: &impl Serialize) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    (code, [(CONTENT_TYPE, "application/json")], body).into_response()
}

#[derive(Serialize)]
struct Refused<'a> {
    error: String,
    #[serde(flatten)]
    view: &'a RoleView,
}

fn answer(control: &Control, done: Result<RoleView, Refusal>) -> Response {
    match done {
        Ok(view) => json(StatusCode::OK, &view),
        Err(Refusal::Role(view, wanted)) => json(
            StatusCode::CONFLICT,
            &Refused {
                error: format!(
                    "this call needs a process that is {wanted}, not {:?}",
                    view.role
                ),
                view: &view,
            },
        ),
        Err(Refusal::Generation(why)) => json(
            StatusCode::CONFLICT,
            &Refused {
                error: why,
                view: &control.view(),
            },
        ),
        Err(Refusal::Failed(why)) => json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &Refused {
                error: why,
                view: &control.view(),
            },
        ),
    }
}

async fn role(State(control): State<Arc<Control>>) -> Response {
    json(StatusCode::OK, &control.view())
}

async fn quiesce(State(control): State<Arc<Control>>) -> Response {
    let done = control.quiesce().await;
    answer(&control, done)
}

async fn resume(State(control): State<Arc<Control>>) -> Response {
    let done = control.resume().await;
    answer(&control, done)
}

async fn stand_down(State(control): State<Arc<Control>>) -> Response {
    let done = control.stand_down().await;
    answer(&control, done)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Takeover {
    /// The generation the holder reports.
    expect: u64,
}

async fn takeover(State(control): State<Arc<Control>>, body: Bytes) -> Response {
    let Ok(Takeover { expect }) = serde_json::from_slice(&body) else {
        return json(
            StatusCode::BAD_REQUEST,
            &serde_json::json!({"error": "the body is {\"expect\": <the holder's generation>}"}),
        );
    };
    let done = control.takeover(expect).await;
    answer(&control, done)
}
