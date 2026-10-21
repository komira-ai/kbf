//! `GET /v1/health`: the one-screen answer to "is the farm healthy", composed from
//! values the server already reports elsewhere, so a page or an agent does not add
//! numbers up (`docs/design/kbf-ui-v1.md`, N6).
//!
//! Every field is a value another route reports, taken from the same source:
//!
//! - `ready` and `failing`: [`Checks::failing`], the evaluation `/readyz` and the
//!   `grpc.health.v1` service use. `/readyz` stays the probe: this route answers `200`
//!   whether or not the server is ready, and says which in `ready`.
//! - `nodes`: [`Farm::nodes`], the list of `GET /v1/nodes`, each node counted once, in
//!   the first of these that holds: `disconnected` (its stream is closed), `quarantined`
//!   (its daemon reported a quarantine), then its placement: `cordoned`, `draining` (a
//!   drain running or paused at its deadline), `drained`, or `serving`. `total` is the
//!   number of nodes listed, and is the sum of the six.
//! - `queue`: `depth` and `oldest_age_ms`, both from one [`Farm::queue_view`] snapshot,
//!   the one `GET /v1/queue` shows (`depth` is also the `queued_operations` of
//!   `GET /v1/frontend/drain`). `oldest_age_ms` is how long the longest-waiting
//!   operation has waited, and `null` only when nothing waits.
//! - `leases`: the leases held on all nodes, the sum of `lease_count` of `GET /v1/nodes`.
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
//! In memory, like `GET /v1/nodes`: after a restart `nodes`, `queue` and `leases` count
//! only what has reconnected since. Not included: the update state and the history
//! store's counters, which this route's sources do not reach.
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
use crate::farm::Farm;
use crate::fleet::{NodeView, PhaseView, PlacementView};
use crate::health::Checks;
use crate::members::Members;

/// The path of `GET /v1/health`.
pub const HEALTH: &str = "/v1/health";

/// What the route reads.
struct Sources {
    checks: Arc<Checks>,
    farm: Arc<Farm>,
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
pub fn route(checks: Arc<Checks>, farm: Arc<Farm>, members: Option<Arc<Members>>) -> Router {
    Router::new()
        .route(HEALTH, get(health))
        .with_state(Arc::new(Sources {
            checks,
            farm,
            members,
            started: Started::now(),
        }))
}

async fn health(State(sources): State<Arc<Sources>>) -> Response {
    let failing = sources.checks.failing().await;
    let nodes = sources.farm.nodes();
    let (by_state, leases) = count(&nodes.nodes);
    let (storage, unavailable) = storage(sources.members.clone()).await;
    // One snapshot of the scheduler's queue, the one `GET /v1/queue` shows, for both numbers.
    let queue = sources.farm.queue_view();
    let mut body = json!({
        "ready": failing.is_empty(),
        "failing": failing
            .iter()
            .map(|f| json!({"check": f.check, "reason": f.reason}))
            .collect::<Vec<_>>(),
        "nodes": by_state,
        "queue": {
            "depth": queue.depth,
            "oldest_age_ms": queue.oldest_age.map(crate::http_sched::millis),
        },
        "leases": leases,
        "storage": storage,
        "started_unix_ms": sources.started.unix_ms,
        "uptime_ms": u64::try_from(sources.started.at.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
    if let Some(why) = unavailable {
        body["storage_unavailable"] = why.into();
    }
    answer(StatusCode::OK, &body)
}

/// How many of `nodes` are in each state (each counted once, as the module says), and
/// the leases they hold in all.
fn count(nodes: &[NodeView]) -> (Value, u64) {
    let mut counts = [0_u64; 6];
    for node in nodes {
        let state = if !node.connected {
            5
        } else if matches!(node.phase, PhaseView::Quarantined { .. }) {
            4
        } else {
            match node.placement {
                PlacementView::Serving => 0,
                PlacementView::Cordoned => 1,
                PlacementView::Draining { .. } | PlacementView::DrainPaused { .. } => 2,
                PlacementView::Drained => 3,
            }
        };
        counts[state] += 1;
    }
    let [
        serving,
        cordoned,
        draining,
        drained,
        quarantined,
        disconnected,
    ] = counts;
    let by_state = json!({
        "total": nodes.len(),
        "serving": serving,
        "cordoned": cordoned,
        "draining": draining,
        "drained": drained,
        "quarantined": quarantined,
        "disconnected": disconnected,
    });
    let leases = nodes.iter().map(|n| u64::from(n.lease_count)).sum();
    (by_state, leases)
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
