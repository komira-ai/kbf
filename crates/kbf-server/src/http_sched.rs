//! `GET /v1/leases` and `GET /v1/queue` on the operator API listener: what the scheduler
//! holds and what waits for it (`docs/design/kbf-ui-v1.md` appendix A, N3 and N4).
//!
//! Both are reads of memory-only state: after a restart they are empty until the daemons
//! reconnect, and a frontend lists only what its own scheduler holds. Like `GET /v1/nodes`
//! they answer anyone who reaches the listener, and nothing is redacted.
//!
//! - **`GET /v1/leases`**: `{"term", "items", "next_page"}`. `term` is the scheduler's
//!   leader term as a string (it is a 64-bit number a JSON reader would round), shared
//!   by every lease and different after a restart. Each item is a lease
//!   (`lease`, which is `term.seq`, `node`, `phase`: `proposed`, `granted` or `running`;
//!   `age_ms` since the grant and `since_ms` since the phase began, `granted_unix_ms`,
//!   `started_unix_ms` or `null`, `operation`, `instance`, `digest`, `platform`, `qos`),
//!   in lease order. `?node=` keeps one node's leases: a node the scheduler does not know
//!   has none, so the answer is an empty list, not an error. `?page_size=` is 1 to
//!   [`MAX_PAGE_SIZE`] (default [`DEFAULT_PAGE_SIZE`]); `?page_token=` is the token of
//!   the page before, taken from `next_page`, bound to the node filter it was made for.
//!   Unknown and repeated query keys, and a value out of range, are `400`.
//! - **`GET /v1/queue`**: `{"depth", "oldest_age_ms", "by_platform", "waiting",
//!   "avoided", "avoided_total"}`. `by_platform` has one entry per platform and QoS
//!   level; `waiting` one per reason no live worker can run the work, with its count
//!   and an example; `avoided` the queued operations placement keeps off some nodes,
//!   at most [`kbf_sched::MAX_AVOIDED`], of which there are `avoided_total`. It takes no query.
//!
//! Every answer is `Cache-Control: no-store`. Durations carry the unit in the key. The
//! routes are written up in `docs/api-scheduler.md`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use axum::response::Response;
use axum::routing::get;
use kbf_sched::{LeaseView, QueueView};
use kbf_types::{LeaseId, WorkerId};
use serde_json::{Value, json};

use crate::api::{error, json};
use crate::farm::Farm;

/// The path of `GET /v1/leases`.
pub const LEASES: &str = "/v1/leases";

/// The path of `GET /v1/queue`.
pub const QUEUE: &str = "/v1/queue";

/// Leases per page when the request names no `page_size`.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// The most leases a page may ask for.
pub const MAX_PAGE_SIZE: usize = 200;

/// The longest `node` filter, in bytes.
pub const MAX_NODE_LEN: usize = 256;

/// The routes over `farm`'s scheduler.
pub fn router(farm: Arc<Farm>) -> Router {
    Router::new()
        .route(LEASES, get(leases))
        .route(QUEUE, get(queue))
        .with_state(farm)
}

/// A request's page of leases.
#[derive(Debug, PartialEq, Eq)]
struct Query {
    node: Option<String>,
    size: usize,
    after: Option<LeaseId>,
}

/// FNV-1a over `bytes`: what binds a page token to its filter. Not a secret, only a
/// check that a token is used with the filter it was made for.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The token of the page after `lease`, for a request filtered by `node`.
fn token(lease: LeaseId, node: Option<&str>) -> String {
    let tag = fnv(node.unwrap_or_default().as_bytes());
    format!("{}.{}.{tag:016x}", lease.term, lease.seq)
}

/// The lease a token made for `node` names; `None` if it is not one of ours or was made
/// for another filter.
fn untoken(token: &str, node: Option<&str>) -> Option<LeaseId> {
    let mut parts = token.splitn(3, '.');
    let term = parts.next()?.parse().ok()?;
    let seq = parts.next()?.parse().ok()?;
    let tag = u64::from_str_radix(parts.next()?, 16).ok()?;
    (tag == fnv(node.unwrap_or_default().as_bytes())).then(|| LeaseId::new(term, seq))
}

impl Query {
    /// The request `query` (after `?`) makes.
    fn parse(query: &str) -> Result<Self, String> {
        let mut parsed = Self {
            node: None,
            size: DEFAULT_PAGE_SIZE,
            after: None,
        };
        let (mut size, mut page_token) = (false, None::<String>);
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let repeated = || format!("the query key {key:?} is repeated");
            match key.as_ref() {
                "node" => {
                    if parsed.node.is_some() {
                        return Err(repeated());
                    }
                    if value.is_empty() || value.len() > MAX_NODE_LEN {
                        return Err(format!("node is a node id of 1 to {MAX_NODE_LEN} bytes"));
                    }
                    parsed.node = Some(value.into_owned());
                }
                "page_size" => {
                    if std::mem::replace(&mut size, true) {
                        return Err(repeated());
                    }
                    parsed.size = value
                        .parse()
                        .ok()
                        .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
                        .ok_or_else(|| {
                            format!("page_size is a whole number from 1 to {MAX_PAGE_SIZE}")
                        })?;
                }
                "page_token" => {
                    if page_token.replace(value.into_owned()).is_some() {
                        return Err(repeated());
                    }
                }
                other => return Err(format!("this route takes no query key {other:?}")),
            }
        }
        if let Some(page_token) = page_token {
            parsed.after = Some(untoken(&page_token, parsed.node.as_deref()).ok_or_else(|| {
                "page_token is not one this route made for this node filter".to_owned()
            })?);
        }
        Ok(parsed)
    }
}

/// An answer that must not be cached: every one of these routes reads live state.
fn fresh(code: StatusCode, value: &Value) -> Response {
    let mut answer = json(code, value);
    answer.headers_mut().insert(
        CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    answer
}

fn bad(why: &str) -> Response {
    let mut answer = error(StatusCode::BAD_REQUEST, why);
    answer.headers_mut().insert(
        CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    answer
}

/// A duration in whole milliseconds, saturating.
pub(crate) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One lease as the API shows it; `epoch_unix_ms` turns its farm times into wall-clock.
fn lease_item(lease: &LeaseView, epoch_unix_ms: u64) -> Value {
    let wall = |at: kbf_types::FarmTime| epoch_unix_ms.saturating_add(at.as_millis());
    json!({
        "lease": lease.lease.to_string(),
        "node": lease.node.as_str(),
        "phase": lease.phase.name(),
        "age_ms": millis(lease.age),
        "since_ms": millis(lease.phase_age),
        "granted_unix_ms": wall(lease.granted_at),
        "started_unix_ms": lease.started_at.map(wall),
        "operation": lease.operation.to_string(),
        "instance": lease.instance,
        "digest": lease.action.to_string(),
        "platform": lease.platform,
        "qos": lease.qos.name(),
    })
}

async fn leases(State(farm): State<Arc<Farm>>, RawQuery(raw): RawQuery) -> Response {
    let query = match Query::parse(raw.as_deref().unwrap_or_default()) {
        Ok(query) => query,
        Err(why) => return bad(&why),
    };
    let node = query.node.as_deref().map(WorkerId::new);
    let views = farm.lease_views(node.as_ref());
    let mut page: Vec<&LeaseView> = views
        .leases
        .iter()
        .filter(|l| query.after.is_none_or(|after| l.lease > after))
        .collect();
    let more = page.len() > query.size;
    page.truncate(query.size);
    let next_page = page.last().filter(|_| more).map(|last| {
        let mut link = form_urlencoded::Serializer::new(format!("{LEASES}?"));
        if let Some(node) = &query.node {
            link.append_pair("node", node);
        }
        link.append_pair("page_size", &query.size.to_string());
        link.append_pair("page_token", &token(last.lease, query.node.as_deref()));
        link.finish()
    });
    let items: Vec<Value> = page
        .iter()
        .map(|l| lease_item(l, views.epoch_unix_ms))
        .collect();
    fresh(
        StatusCode::OK,
        &json!({
            "term": views.term.to_string(),
            "items": items,
            "next_page": next_page,
        }),
    )
}

/// The queue as the API shows it.
fn queue_body(queue: &QueueView) -> Value {
    let by_platform: Vec<Value> = queue
        .groups
        .iter()
        .map(|g| {
            json!({
                "platform": g.platform,
                "qos": g.qos.name(),
                "depth": g.depth,
                "oldest_age_ms": millis(g.oldest_age),
            })
        })
        .collect();
    let waiting: Vec<Value> = queue
        .waiting
        .iter()
        .map(|g| {
            json!({
                "reason": g.reason,
                "count": g.count,
                "example_operation": g.example.to_string(),
                "example_instance": g.example_instance,
                "example_digest": g.example_action.to_string(),
            })
        })
        .collect();
    let avoided: Vec<Value> = queue
        .avoided
        .iter()
        .map(|a| {
            json!({
                "operation": a.operation.to_string(),
                "instance": a.instance,
                "digest": a.action.to_string(),
                "nodes": a.nodes.iter().map(WorkerId::as_str).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "depth": queue.depth,
        "oldest_age_ms": queue.oldest_age.map(millis),
        "by_platform": by_platform,
        "waiting": waiting,
        "avoided": avoided,
        "avoided_total": queue.avoided_total,
    })
}

async fn queue(State(farm): State<Arc<Farm>>, RawQuery(raw): RawQuery) -> Response {
    if let Some(raw) = raw.filter(|raw| !raw.is_empty()) {
        let key = form_urlencoded::parse(raw.as_bytes())
            .next()
            .map(|(key, _)| key.into_owned())
            .unwrap_or_default();
        return bad(&format!("this route takes no query key {key:?}"));
    }
    fresh(StatusCode::OK, &queue_body(&farm.queue_view()))
}

#[cfg(test)]
mod tests;
