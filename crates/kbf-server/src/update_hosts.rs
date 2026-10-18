//! `GET /v1/update/hosts` on the operator API listener: what each host last said about
//! its update, with the farm's desired commit and whether the farm is paused
//! (`docs/design/update-ui.md` section 5, amendment A5; `docs/api.md`).
//!
//! - **Off unless the policy names `farm.getAuthorizer`.** The route runs the REAPI
//!   policy (`--reapi-auth-policy`): its authenticator on the request, then its
//!   `farm.getAuthorizer` ([`kbf_auth::Policy::farm_get`]) with the empty instance name.
//!   Without that key, or without a policy file, every request is `404` with
//!   `"off": "farm"`. The open per-node summary is on `GET /v1/nodes`; this route
//!   carries what that one must not: the commits each host runs, stages and installed,
//!   and its refusal text.
//! - **The answer**: `{"desired", "paused", "hosts", "next_page_token"}`. `desired` is
//!   the row of `GET /v1/update` (`null` before any offer, or without `--state-store`).
//!   `paused` follows [`effective_paused`]. Each host is a [`HostUpdate`], in node-id
//!   order.
//! - **Pages**: `?page_size=` (100 by default, at most 1000) and the `next_page_token`
//!   of the page before, which names the last node of that page; a node that registers
//!   meanwhile is listed where its id falls. Any other query key is `400`.
//! - **A state store that fails** is `500`, not hosts without a desired commit.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::get;
use kbf_auth::{Authenticator, Authorizer, Policy, authorize};
use serde_json::{Value, json};

use crate::api::{error, json};
use crate::farm::Farm;
use crate::http_blobs::{off, refusal};
use crate::release::desired::{blocking, desired_view, policy_view, row_failed};
use crate::update::Rows;

/// The path of `GET /v1/update/hosts`.
pub const HOSTS: &str = "/v1/update/hosts";

/// Hosts per page when the request names no `page_size`.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// The most hosts a page may ask for.
pub const MAX_PAGE_SIZE: usize = 1000;

/// The call name refusals are logged with.
const CALL: &str = "GET /v1/update/hosts";

/// Who may read the per-host update detail: the REAPI policy's authenticator and its
/// `farm.getAuthorizer`.
#[derive(Clone)]
pub struct FarmReaders {
    authenticator: Arc<dyn Authenticator>,
    authorizer: Arc<dyn Authorizer>,
}

impl FarmReaders {
    /// The readers `policy` allows, or `None` (the route is off) when it names no
    /// `farm.getAuthorizer`.
    #[must_use]
    pub fn from_policy(policy: &Policy) -> Option<Self> {
        Some(Self {
            authenticator: Arc::clone(&policy.authenticator),
            authorizer: Arc::clone(policy.farm_get.as_ref()?),
        })
    }
}

struct Hosts {
    farm: Arc<Farm>,
    rows: Option<Rows>,
    readers: Option<FarmReaders>,
}

/// The route over `farm`'s hosts; `rows` is where the desired release lives (`None`
/// without a state store); with `readers` `None` every request is `404`.
pub fn router(farm: Arc<Farm>, rows: Option<Rows>, readers: Option<FarmReaders>) -> Router {
    Router::new()
        .route(HOSTS, get(hosts))
        .with_state(Arc::new(Hosts {
            farm,
            rows,
            readers,
        }))
}

/// Whether the farm is paused, as a reader that must not call a paused farm running
/// takes it: `policy.paused` when the policy object has that key as a boolean; `None`
/// (unknown, never "running") when the object has not, or the key is not a boolean; and
/// `false` without a policy row, as `GET /v1/update` says (the pause lives in the policy
/// row alone, and without one the defaults hold). `policy` is the policy as that route
/// shows it (`null` before a policy row exists).
#[must_use]
pub fn effective_paused(policy: &Value) -> Option<bool> {
    if policy.is_null() {
        return Some(false);
    }
    policy.get("paused").and_then(Value::as_bool)
}

/// A request's page: how many hosts, and after which node.
struct Page {
    size: usize,
    after: Option<String>,
}

impl Page {
    /// The page `query` (after `?`) asks for.
    fn parse(query: &str) -> Result<Self, String> {
        let mut page = Self {
            size: DEFAULT_PAGE_SIZE,
            after: None,
        };
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "page_size" => {
                    page.size = value
                        .parse()
                        .ok()
                        .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
                        .ok_or_else(|| {
                            format!("page_size is a whole number from 1 to {MAX_PAGE_SIZE}")
                        })?;
                }
                "page_token" => page.after = Some(value.into_owned()),
                other => return Err(format!("this route takes no query key {other:?}")),
            }
        }
        Ok(page)
    }
}

async fn hosts(State(state): State<Arc<Hosts>>, parts: Parts) -> Response {
    let Some(readers) = &state.readers else {
        return off(
            "farm",
            "the per-host update detail is off: the REAPI policy names no farm.getAuthorizer",
        );
    };
    let caller = match readers.authenticator.authenticate(&parts).await {
        Ok(caller) => caller,
        Err(e) => {
            tracing::warn!(call = CALL, error = %e.message(), "farm read not authenticated");
            return refusal(&e);
        }
    };
    if let Err(e) = authorize(&*readers.authorizer, &caller, CALL, "").await {
        return refusal(&e);
    }
    let page = match Page::parse(parts.uri.query().unwrap_or_default()) {
        Ok(page) => page,
        Err(why) => return error(StatusCode::BAD_REQUEST, &why),
    };
    let (desired, policy) = match &state.rows {
        None => (None, None),
        Some(rows) => {
            let (desired_rows, policy_rows) = (rows.clone(), rows.clone());
            let desired = match blocking(move || desired_rows.desired()).await {
                Ok(stored) => stored.value,
                Err(e) => return row_failed(&e),
            };
            let policy = match blocking(move || policy_rows.policy_if_set()).await {
                Ok(policy) => policy,
                Err(e) => return row_failed(&e),
            };
            (desired, policy)
        }
    };
    let desired_json = desired_view(desired.as_ref());
    let policy_json = policy.as_ref().map_or(Value::Null, policy_view);
    let commit = desired.as_ref().map(|d| d.commit.as_str());
    let mut all = state.farm.update_hosts();
    all.retain(|host| {
        page.after
            .as_ref()
            .is_none_or(|after| host.node_id > *after)
    });
    let more = all.len() > page.size;
    all.truncate(page.size);
    for host in &mut all {
        host.judge(commit);
    }
    let next = all.last().filter(|_| more).map(|host| host.node_id.clone());
    json(
        StatusCode::OK,
        &json!({
            "desired": desired_json,
            "paused": effective_paused(&policy_json),
            "hosts": all,
            "next_page_token": next,
        }),
    )
}

#[cfg(test)]
mod tests;
