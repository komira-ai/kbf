//! `GET /v1/health`: the one-screen answer to "is the farm healthy", composed from
//! values the server already reports elsewhere, so a page or an agent does not add
//! numbers up (`docs/design/kbf-ui-v1.md`, N6).
//!
//! Every field is a value another route reports, taken from the same source:
//!
//! - `ready` and `failing`: [`Checks::failing`], the evaluation `/readyz` and the
//!   `grpc.health.v1` service use. `/readyz` stays the probe: this route answers `200`
//!   whether or not the server is ready, and says which in `ready`.
//! - `storage`: [`crate::members::Members::storage_health`], the numbers of
//!   `GET /v1/storage`; `null` when this server keeps no storage members, and when the
//!   membership cannot be read (`storage_unavailable` then says why).
//!   `under_replicated_blobs` is `null` while unknown, never zero.
//!
//! - `started_unix_ms` and `uptime_ms`: when this route was made (the server's start, or
//!   its frontend's after a swap: when the in-memory state below began to fill), as wall
//!   clock milliseconds since the Unix epoch, and the milliseconds since by the monotonic
//!   clock, so a clock step never shows in `uptime_ms`.
//!
//! Not included: the update state and the history store's counters, which this route's
//! sources do not reach; nor anything about the jobs, which are the job service's.
//!
//! Open, like the other reads of this listener: it names no secret.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};

use crate::api::json as answer;
use crate::health::Checks;
use crate::members::Members;

/// The path of `GET /v1/health`.
pub const HEALTH: &str = "/v1/health";

/// What the route reads.
struct Sources {
    checks: Arc<Checks>,
    members: Option<Arc<Members>>,
    started: Started,
}

/// When this route was made: the server's start, or its frontend's after a swap, which is
/// when the in-memory state it reports began to fill.
#[derive(Clone, Copy, Debug)]
struct Started {
    /// Wall clock, milliseconds since the Unix epoch.
    unix_ms: u64,
    /// The monotonic reading `uptime_ms` counts from, so a clock step never shows.
    at: Instant,
}

impl Started {
    /// Now.
    fn now() -> Self {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        Self {
            unix_ms,
            at: Instant::now(),
        }
    }
}

/// `GET /v1/health` over these sources; the start it reports is the moment of this call.
pub fn route(checks: Arc<Checks>, members: Option<Arc<Members>>) -> Router {
    Router::new()
        .route(HEALTH, get(health))
        .with_state(Arc::new(Sources {
            checks,
            members,
            started: Started::now(),
        }))
}

async fn health(State(sources): State<Arc<Sources>>) -> Response {
    let failing = sources.checks.failing().await;
    let (storage, unavailable) = storage(sources.members.clone()).await;
    let mut body = json!({
        "ready": failing.is_empty(),
        "failing": failing
            .iter()
            .map(|f| json!({"check": f.check, "reason": f.reason}))
            .collect::<Vec<_>>(),
        "storage": storage,
        "started_unix_ms": sources.started.unix_ms,
        "uptime_ms": u64::try_from(sources.started.at.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
    if let Some(why) = unavailable {
        body["storage_unavailable"] = why.into();
    }
    answer(StatusCode::OK, &body)
}

/// The storage numbers, or `null` and why they are not there.
async fn storage(members: Option<Arc<Members>>) -> (Value, Option<String>) {
    let Some(members) = members else {
        return (Value::Null, None);
    };
    let read = tokio::task::spawn_blocking(move || members.storage_health()).await;
    match read {
        Ok(Ok(health)) => (
            json!({
                "members_active": health.members_active,
                "members_total": health.members_total,
                "under_replicated_blobs": health.under_replicated_blobs,
            }),
            None,
        ),
        Ok(Err(e)) => (Value::Null, Some(e.to_string())),
        Err(e) => (Value::Null, Some(format!("storage health task: {e}"))),
    }
}
