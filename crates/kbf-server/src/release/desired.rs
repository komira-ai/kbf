//! The desired release (`docs/design/pull-updates.md` section 6.3, over the `update/desired`
//! row of [`crate::update::Rows`]): `GET /v1/update`, `POST /v1/update/desired`, `POST
//! /v1/update/pause` and `POST /v1/update/resume`.
//!
//! - `GET /v1/update` is open like `GET /v1/nodes`: the desired release (`null` before any),
//!   and the commits the release store holds with whether each platform is complete. Its
//!   `proposals` are the releases the poller (`release::poll`) found, oldest first: a
//!   proposal starts nothing.
//! - `POST /v1/update/desired {"commit": "<40 hex>"}` offers the commit: the store must hold
//!   it complete (tarball and attestation) for at least one platform, else `409` naming
//!   what is missing. The row's generation moves only when the commit or the source differs
//!   from the desired one (a repeated click keeps it); a rollback is the older commit offered
//!   the same way. R4 refuses a release older than this build by build seq
//!   ([`super::target_seq`]) unless the body says `"rollback": true`, which skips only that
//!   check. It passes the gates of every `/v1` write ([`crate::api::operator_gate`]) and wants a
//!   JSON body.
//!   An optional `expect_generation` makes it a compare-and-set: the row is written only if the
//!   desired release is at that generation (0 before any offer), else `409` naming both and
//!   nothing is written. `GET` also shows the `policy` row when the operator wrote one.
//! - An optional `"source"` is `"fetch"` (absent means it: what every sender meant before the
//!   field) or `"staged"`; any other value is `400`. The checks above are those of a fetch. A
//!   staged offer skips the release store and R4 and wants one thing: the commit is this
//!   build's own ([`crate::update::age::same_commit`] on the build's full commit, else on the
//!   12 digits it has), else `409` with `"code": "not_own"` and nothing written. The proxy
//!   posts it in step 9, after the child it started took the generation, so the hosts use the
//!   release their package carries; a server started without `--releases-dir` takes it. `GET`
//!   shows the `source` of the desired release.
//! - `POST /v1/update/pause` and `resume` set `paused` of the policy row (a compare-and-set of
//!   that one field): while set, no host is started. It needs no run, and survives the next offer.
//! - Without a state store (`--state-store`: the rows have nowhere to live) every answer is
//!   `503` saying so. Without a release store (`--releases-dir`) `GET /v1/update` and `POST
//!   /v1/update/desired` of a fetch are `503` saying so; `pause` and `resume` touch only the
//!   policy row and work, and so does a staged offer.
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
use super::target_seq::{AgeCheck, running_commit};
use crate::api::{error, error_coded, json, operator_gate};
use crate::farm::Farm;
use crate::proxy_auth::Caller;
use crate::token::ApiToken;
use crate::update::{Desired, Policy, Proposal, Release, RowError, Rows, Source, age};

/// The code of the `409` for a staged offer of a commit that is not this build's own. The
/// proxy's admin client keys on it (`kbf-installer`, `proxy/admin.rs`).
const NOT_OWN: &str = "not_own";

/// The release store and the R4 check over it, present together: what a fetch needs.
struct Held {
    store: Arc<ReleaseStore>,
    age: AgeCheck,
}

struct Update {
    held: Option<Held>,
    rows: Option<Rows>,
    token: Option<ApiToken>,
    farm: Arc<Farm>,
    /// The commit of this build ([`running_commit`]): what a staged offer must name. It is
    /// kept outside `held` because a staged offer needs no release store.
    own_commit: String,
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
            held: store.map(|store| Held {
                age: AgeCheck::running(store.clone()),
                store,
            }),
            rows,
            token,
            farm,
            own_commit: running_commit().to_owned(),
        }))
}

fn unavailable(what: &str, flag: &str) -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("{what} is off: the server was started without {flag}"),
    )
}

/// A read of the release store failed: logged, and a `500` that says nothing of the path.
fn store_failed(e: &std::io::Error) -> Response {
    tracing::error!("release store: {e}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "the release store failed",
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
            "source": d.source,
        })
    })
}

/// A proposal of the release poller as the operator sees it. A proposal starts nothing: the
/// operator offers the commit with `POST /v1/update/desired`.
fn proposal_view(p: &Proposal) -> serde_json::Value {
    json!({
        "commit": p.commit,
        "tag": p.tag,
        "published_at_secs": p.published_at_secs,
        "proposed_at_ms": p.proposed_at_ms,
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
    let Some(Held { store, .. }) = update.held.as_ref() else {
        return unavailable("the release store", "--releases-dir");
    };
    let policy_rows = rows.clone();
    let proposal_rows = rows.clone();
    let desired = match blocking(move || rows.desired()).await {
        Ok(stored) => stored.value,
        Err(e) => return row_failed(&e),
    };
    let policy = match blocking(move || policy_rows.policy_if_set()).await {
        Ok(policy) => policy,
        Err(e) => return row_failed(&e),
    };
    let proposals = match blocking(move || proposal_rows.proposals()).await {
        Ok(held) => held,
        Err(e) => return row_failed(&e),
    };
    // Without a policy row nothing is paused (the defaults).
    let paused = policy.as_ref().is_some_and(|p| p.paused);
    let releases = match store.list().await {
        Ok(listing) => super::http::listing_view(&listing),
        Err(e) => return store_failed(&e),
    };
    json(
        StatusCode::OK,
        &json!({
            "desired": desired_view(desired.as_ref()),
            "paused": paused,
            "policy": policy.as_ref().map_or(serde_json::Value::Null, policy_view),
            "proposals": proposals.iter().map(proposal_view).collect::<Vec<_>>(),
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
    /// An older release on purpose: skips R4, the age check, and nothing else.
    #[serde(default)]
    rollback: bool,
    /// Where the hosts get the release; absent is a fetch, as every offer meant before.
    #[serde(default)]
    source: Source,
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
    let Ok(Offer {
        commit,
        expect_generation,
        rollback,
        source,
    }) = serde_json::from_slice::<Offer>(&body)
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "the body must be {\"commit\": \"<40 hex>\"}, and \"source\" may be \"fetch\" or \
             \"staged\"",
        );
    };
    let Some(parsed) = Commit::parse(&commit) else {
        return error(
            StatusCode::BAD_REQUEST,
            "commit must be 40 lowercase hex digits",
        );
    };
    // What the offer needs depends on where the release comes from: a fetch, the release store;
    // a staged release, the package of the host, which the server only knows to be its own.
    let refusal = match source {
        Source::Fetch => fetch_refusal(&update, &parsed, rollback).await,
        Source::Staged => staged_refusal(&update, &parsed),
    };
    if let Some(refusal) = refusal {
        return refusal;
    }
    // The wall clock: the controller counts its waits from this, across restarts.
    let now = update.farm.wall_now();
    let release = Release::new(commit);
    let write = move || rows.offer_expecting(&release, source, now, expect_generation);
    match blocking(write).await {
        Ok(desired) => json(StatusCode::OK, &desired_view(Some(&desired))),
        Err(e @ RowError::Stale { .. }) => error(StatusCode::CONFLICT, &e.to_string()),
        Err(e) => row_failed(&e),
    }
}

/// What a fetch needs, or the refusal: a release store (`503` without), that holds `commit`
/// complete for some platform (`409`), and R4 unless `rollback` (`409` coded).
async fn fetch_refusal(update: &Update, commit: &Commit, rollback: bool) -> Option<Response> {
    let Some(Held { store, age }) = update.held.as_ref() else {
        return Some(unavailable("the release store", "--releases-dir"));
    };
    let listing = match store.list().await {
        Ok(listing) => listing,
        Err(e) => return Some(store_failed(&e)),
    };
    let complete = listing
        .iter()
        .filter(|c| c.commit == *commit)
        .flat_map(|c| &c.platforms)
        .any(|p| p.complete);
    if !complete {
        return Some(error(
            StatusCode::CONFLICT,
            "the release store holds no platform of this commit complete (tarball and attestation)",
        ));
    }
    // R4: not older than the running build, by build seq.
    // A rollback is the explicit exception; it still needs a complete release.
    let aged = if rollback {
        Ok(Ok(()))
    } else {
        age.check(commit).await
    };
    match aged {
        Ok(Ok(())) => None,
        Ok(Err(refused)) => {
            let code = refused.refusal.code();
            let response = error_coded(StatusCode::CONFLICT, &refused.to_string(), code);
            Some(response)
        }
        Err(e) => Some(store_failed(&e)),
    }
}

/// What a staged offer needs, or the refusal: `commit` is this build's own, whatever the
/// release store holds and whatever R4 would say (`409` coded [`NOT_OWN`]). A build that knows
/// its full commit compares all 40 digits; one that knows 12 compares those, as R4 does for
/// its own commit ([`same_commit`](age::same_commit)).
fn staged_refusal(update: &Update, commit: &Commit) -> Option<Response> {
    if age::same_commit(&update.own_commit, commit.as_str()) {
        return None;
    }
    let why = format!(
        "{} is not the release of this server ({}): a staged update takes only that",
        commit.as_str(),
        update.own_commit
    );
    Some(error_coded(StatusCode::CONFLICT, &why, NOT_OWN))
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

#[cfg(test)]
mod tests;
