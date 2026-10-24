//! `GET` and `POST /v1/frontend/retire`: the status of the frontend's retire mode, and
//! beginning it ([`crate::retire`]; `docs/api.md`).
//!
//! The read is open, like the drain's. The write passes the gates of [`gate`] and takes no
//! body, or `{}`. A server that is not a frontend over a state store has nothing to retire
//! over and answers both `409`: the cell the handlers read is set only by a frontend that
//! built a [`Stand`](crate::stand::Stand), which it does with a state store.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

use super::{ApiState, NoBody, error, gate, json};
use crate::proxy_auth::Caller;

/// The answer of a server that has no retire mode.
fn no_state_store() -> Response {
    error(
        StatusCode::CONFLICT,
        "this server is not a frontend over a state store: a frontend retires only over one it \
         shares with others",
    )
}

/// `GET /v1/frontend/retire`: the retire as it is now.
pub(super) async fn status(State(api): State<Arc<ApiState>>) -> Response {
    let Some(retire) = api.retire.get() else {
        return no_state_store();
    };
    json(StatusCode::OK, &retire.status())
}

/// `POST /v1/frontend/retire`: begins the retire, and answers when it has ended with the
/// status. A second call answers the same status.
pub(super) async fn begin(
    State(api): State<Arc<ApiState>>,
    peer: Caller,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = gate(api.token.as_ref(), &peer, &headers) {
        return error(code, &why);
    }
    if !body.is_empty()
        && let Err(e) = serde_json::from_slice::<NoBody>(&body)
    {
        let why = format!("a frontend retire takes no body, or {{}}: {e}");
        return error(StatusCode::BAD_REQUEST, &why);
    }
    let Some(retire) = api.retire.get() else {
        return no_state_store();
    };
    if retire.begin().await {
        tracing::info!("frontend retired by the operator API");
    }
    json(StatusCode::OK, &retire.status())
}
