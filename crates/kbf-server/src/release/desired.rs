//! The desired release (`docs/design/pull-updates.md` section 6.3, over the `update/desired`
//! row of [`crate::update::Rows`]): `GET /v1/update`, `POST /v1/update/desired`, `POST
//! /v1/update/pause` and `POST /v1/update/resume`.
//!
//! - `GET /v1/update` is open like `GET /v1/nodes`: the desired release (`null` before any),
//!   and the commits the release store holds with whether each platform is complete.
//! - `POST /v1/update/desired {"commit": "<40 hex>"}` offers the commit: the store must hold
//!   it complete (tarball and attestation) for at least one platform, else `409` naming
//!   what is missing. The row's generation moves only when the commit differs from the
//!   desired one (a repeated click keeps it); a rollback is the older commit offered the same
//!   way. It passes the gates of every `/v1` write ([`crate::api::operator_gate`]) and wants a
//!   JSON body.
//!   An optional `expect_generation` makes it a compare-and-set: the row is written only if the
//!   desired release is at that generation (0 before any offer), else `409` naming both and
//!   nothing is written. `GET` also shows the `policy` row when the operator wrote one.
//! - `POST /v1/update/pause` and `resume` set `paused` of the policy row (a compare-and-set of
//!   that one field): while set, no host is started. It needs no run, and survives the next offer.
//! - Without a state store (`--state-store`: the rows have nowhere to live) every answer is
//!   `503` saying so. Without a release store (`--releases-dir`) `GET /v1/update` and `POST
//!   /v1/update/desired` are `503` saying so; `pause` and `resume` touch only the policy row
//!   and work.
//! - A write that other writers kept racing for all of its retries is `503` with
//!   `Retry-After: 1`. Generations increase and may skip numbers (a write that loses a race
//!   has drawn a counter value).
//!
//! Serve the router with `into_make_service_with_connect_info`, as the API's.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use super::key::Commit;
use super::store::ReleaseStore;
use crate::api::{error, json, operator_gate};
use crate::farm::Farm;
use crate::proxy_auth::Caller;
use crate::token::ApiToken;
use crate::update::{Desired, Policy, Release, RowError, Rows};

struct Update {
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
}

/// The routes. `store` and `rows` are each `None` when the server has none: the routes then
/// answer `503`.
pub fn router(
    store: Option<Arc<ReleaseStore>>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
) -> Router {
    Router::new()
        .route("/v1/update", get(view))
        .route("/v1/update/desired", post(set_desired))
        .route("/v1/update/pause", post(pause))
        .route("/v1/update/resume", post(resume))
        .with_state(Arc::new(Update {
            store,
            rows,
            token,
            farm,
        }))
}

fn unavailable(what: &str, flag: &str) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("{what} is off: the server was started without {flag}"),
    )
}

pub(crate) fn row_failed(e: &RowError) -> Response {
    if matches!(e, RowError::Contended(_)) {
        tracing::warn!("update rows: {e}");
        let mut busy = error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the update rows are busy: send it again",
        );
        busy.headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static("1"));
        return busy;
    }
    tracing::error!("update rows: {e}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "the update rows failed")
}

pub(crate) fn desired_view(desired: Option<&Desired>) -> serde_json::Value {
    desired.map_or(serde_json::Value::Null, |d| {
        json!({
            "generation": d.generation.0,
            "commit": d.commit.as_str(),
            "set_at_ms": d.set_at.as_millis(),
        })
    })
}

/// The policy row as the operator sees it: durations in seconds.
pub(crate) fn policy_view(p: &Policy) -> serde_json::Value {
    let secs = |d: std::time::Duration| d.as_secs();
    json!({
        "paused": p.paused,
        "auto_nodes": p.auto_nodes,
        "auto_frontend": p.auto_frontend,
        "new_work_only_to_updated": p.new_work_only_to_updated,
        "accept_failing_builds": p.accept_failing_builds,
        "idle_wait_secs": secs(p.idle_wait),
        "min_serving": p.min_serving,
        "max_hosts_out": p.max_hosts_out,
        "lease_limit_secs": secs(p.lease_limit),
        "mac_lease_limit_secs": secs(p.mac_lease_limit),
        "write_limit_secs": secs(p.write_limit),
        "activate_limit_secs": secs(p.activate_limit),
        "health_limit_secs": secs(p.health_limit),
        "rebuild_limit_secs": p.rebuild_limit.map(secs),
        "repair_wait_secs": secs(p.repair_wait),
        "mac_force_after_secs": p.mac_force_after.map(secs),
    })
}

/// Runs a blocking call on the store (the rows block on it). A call that panics fails the
/// request as a store error does.
pub(crate) async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, RowError> + Send + 'static,
) -> Result<T, RowError> {
    let joined = tokio::task::spawn_blocking(call).await;
    joined.unwrap_or_else(|e| {
        Err(RowError::Store(kbf_state::StoreError::Failed(
            e.to_string(),
        )))
    })
}

async fn view(State(update): State<Arc<Update>>) -> Response {
    let Some(rows) = update.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    let Some(store) = update.store.as_deref() else {
        return unavailable("the release store", "--releases-dir");
    };
    let policy_rows = rows.clone();
    let desired = match blocking(move || rows.desired()).await {
        Ok(stored) => stored.value,
        Err(e) => return row_failed(&e),
    };
    let policy = match blocking(move || policy_rows.policy_if_set()).await {
        Ok(policy) => policy,
        Err(e) => return row_failed(&e),
    };
    // Without a policy row nothing is paused (the defaults).
    let paused = policy.as_ref().is_some_and(|p| p.paused);
    let releases = match store.list().await {
        Ok(listing) => super::http::listing_view(&listing),
        Err(e) => {
            tracing::error!("release store: list: {e}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the release store failed",
            );
        }
    };
    json(
        StatusCode::OK,
        &json!({
            "desired": desired_view(desired.as_ref()),
            "paused": paused,
            "policy": policy.as_ref().map_or(serde_json::Value::Null, policy_view),
            "releases": releases,
        }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offer {
    commit: String,
    /// Compare-and-set: write only if the desired release is at this generation (0 before any).
    expect_generation: Option<u64>,
}

/// The gates of a write, then the JSON content type for a body.
fn write_gate(
    update: &Update,
    peer: &Caller,
    headers: &HeaderMap,
    wants_json: bool,
) -> Result<(), (StatusCode, String)> {
    operator_gate(update.token.as_ref(), peer, headers)?;
    let json = headers.get(CONTENT_TYPE).map(|v| v.as_bytes());
    if wants_json && json != Some(b"application/json") {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "the body must be JSON: Content-Type: application/json".to_owned(),
        ));
    }
    Ok(())
}

async fn set_desired(
    State(update): State<Arc<Update>>,
    peer: Caller,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err((code, why)) = write_gate(&update, &peer, &headers, true) {
        return error(code, &why);
    }
    let Some(rows) = update.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    let Some(store) = update.store.as_deref() else {
        return unavailable("the release store", "--releases-dir");
    };
    let Ok(Offer {
        commit,
        expect_generation,
    }) = serde_json::from_slice::<Offer>(&body)
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "the body must be {\"commit\": \"<40 hex>\"}",
        );
    };
    let Some(parsed) = Commit::parse(&commit) else {
        return error(
            StatusCode::BAD_REQUEST,
            "commit must be 40 lowercase hex digits",
        );
    };
    let listing = match store.list().await {
        Ok(listing) => listing,
        Err(e) => {
            tracing::error!("release store: list: {e}");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the release store failed",
            );
        }
    };
    let complete = listing
        .iter()
        .filter(|c| c.commit == parsed)
        .flat_map(|c| &c.platforms)
        .any(|p| p.complete);
    if !complete {
        return error(
            StatusCode::CONFLICT,
            "the release store holds no platform of this commit complete (tarball and attestation)",
        );
    }
    let now = update.farm.now();
    match blocking(move || rows.offer_expecting(&Release::new(commit), now, expect_generation))
        .await
    {
        Ok(desired) => json(StatusCode::OK, &desired_view(Some(&desired))),
        Err(e @ RowError::Stale { .. }) => error(StatusCode::CONFLICT, &e.to_string()),
        Err(e) => row_failed(&e),
    }
}

async fn pause(state: State<Arc<Update>>, peer: Caller, headers: HeaderMap) -> Response {
    paused(state, peer, headers, true).await
}

async fn resume(state: State<Arc<Update>>, peer: Caller, headers: HeaderMap) -> Response {
    paused(state, peer, headers, false).await
}

async fn paused(
    State(update): State<Arc<Update>>,
    peer: Caller,
    headers: HeaderMap,
    paused: bool,
) -> Response {
    if let Err((code, why)) = write_gate(&update, &peer, &headers, false) {
        return error(code, &why);
    }
    let Some(rows) = update.rows.clone() else {
        return unavailable("the update state", "--state-store");
    };
    match blocking(move || rows.set_paused(paused)).await {
        Ok(()) => json(StatusCode::OK, &json!({ "paused": paused })),
        Err(e) => row_failed(&e),
    }
}
