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
//!   at most [`kbf_sched::MAX_AVOIDED`], of which there are `avoided_total`. With
//!   `--whole-node-max` it has a `whole_node` section too: the caps' numbers and the
//!   queued whole-machine requests with why each waits; without the option the document
//!   is as it was. It takes no query.
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
use kbf_sched::{LeaseView, QueueView, WHOLE_REASONS, WholeState, whole_reason_name};
use kbf_types::{LeaseId, WorkerId};
use serde_json::{Value, json};

use crate::api::{error, json};
use crate::farm::Farm;
use crate::farm::sched_view::WholeView;

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
        "instance": lease.key.class,
        "digest": lease.key.dedup,
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

/// A farm time `at` as the milliseconds from `now` until it: 0 for a time already past,
/// `null` for the end of time (a hold with no wall time to give it up at).
fn until(at: kbf_types::FarmTime, now: kbf_types::FarmTime) -> Value {
    if at.as_millis() == u64::MAX {
        return Value::Null;
    }
    json!(at.as_millis().saturating_sub(now.as_millis()))
}

/// The `whole_node` section of the queue: the caps' numbers and each queued
/// whole-machine request with why it waits.
fn whole_body(view: &WholeView) -> Value {
    let now = view.now;
    let waiting: serde_json::Map<String, Value> = WHOLE_REASONS
        .iter()
        .map(|r| {
            let count = view.summary.waiting.get(r).copied().unwrap_or(0);
            (whole_reason_name(*r).to_owned(), json!(count))
        })
        .collect();
    let requests: Vec<Value> = view
        .requests
        .iter()
        .map(|r| {
            let mut item = json!({
                "operation": r.operation.to_string(),
                "instance": r.key.class,
                "digest": r.key.dedup,
                "state": "queued",
                "reason": whole_reason_name(r.state.reason()),
                "may_hold_in_ms": null,
                "worker": null,
                "held_for_ms": null,
                "deadline_in_ms": null,
                "give_ups": null,
                "draining": null,
                "barred": [],
                "unservable": null,
            });
            let fields = item.as_object_mut().expect("an object");
            match &r.state {
                WholeState::Early { may_hold_at } => {
                    fields.insert("state".into(), json!("early"));
                    fields.insert("may_hold_in_ms".into(), until(*may_hold_at, now));
                }
                WholeState::Holding {
                    worker,
                    made,
                    deadline,
                    give_ups,
                    draining,
                } => {
                    fields.insert("state".into(), json!("holding"));
                    fields.insert("worker".into(), json!(worker.as_str()));
                    fields.insert(
                        "held_for_ms".into(),
                        json!(now.as_millis().saturating_sub(made.as_millis())),
                    );
                    fields.insert(
                        "deadline_in_ms".into(),
                        deadline.map_or(Value::Null, |at| until(at, now)),
                    );
                    fields.insert("give_ups".into(), json!(give_ups));
                    fields.insert("draining".into(), json!(draining));
                }
                WholeState::Barred { workers, give_ups } => {
                    fields.insert("state".into(), json!("barred"));
                    fields.insert("give_ups".into(), json!(give_ups));
                    let barred: Vec<Value> = workers
                        .iter()
                        .map(|(worker, end)| {
                            json!({"worker": worker.as_str(), "until_in_ms": until(*end, now)})
                        })
                        .collect();
                    fields.insert("barred".into(), json!(barred));
                }
                WholeState::Stopped(_) => {
                    fields.insert("state".into(), json!("stopped"));
                }
                WholeState::Queued => {}
                WholeState::Unservable { reason, .. } => {
                    fields.insert("state".into(), json!("unservable"));
                    fields.insert("unservable".into(), json!(reason));
                }
            }
            item
        })
        .collect();
    let s = &view.summary;
    json!({
        "held": s.held,
        "marks": s.marks,
        "whole": s.whole,
        "draining": s.draining,
        "ceiling": s.ceiling,
        "max_whole_node": s.max_whole_node,
        "capable_hosts": s.capable_hosts,
        "keep_for_ordinary": s.keep_for_ordinary,
        "max_draining": s.max_draining,
        "waiting": waiting,
        "requests": requests,
        "requests_total": view.total,
    })
}

/// The queue as the API shows it; `whole` is its `whole_node` section, present only with
/// the whole-node option on.
fn queue_body(queue: &QueueView, whole: Option<&WholeView>) -> Value {
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
                "example_instance": g.example_key.class,
                "example_digest": g.example_key.dedup,
            })
        })
        .collect();
    let avoided: Vec<Value> = queue
        .avoided
        .iter()
        .map(|a| {
            json!({
                "operation": a.operation.to_string(),
                "instance": a.key.class,
                "digest": a.key.dedup,
                "nodes": a.nodes.iter().map(WorkerId::as_str).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut body = json!({
        "depth": queue.depth,
        "oldest_age_ms": queue.oldest_age.map(millis),
        "by_platform": by_platform,
        "waiting": waiting,
        "avoided": avoided,
        "avoided_total": queue.avoided_total,
    });
    if let Some(whole) = whole {
        body["whole_node"] = whole_body(whole);
    }
    body
}

/// The body of `GET /v1/queue` now: the queue and, with the whole-node option on, its
/// `whole_node` section, read together.
pub(crate) fn queue_document(farm: &Farm) -> Value {
    let (queue, whole) = farm.queue_views();
    queue_body(&queue, whole.as_ref())
}

async fn queue(State(farm): State<Arc<Farm>>, RawQuery(raw): RawQuery) -> Response {
    if let Some(raw) = raw.filter(|raw| !raw.is_empty()) {
        let key = form_urlencoded::parse(raw.as_bytes())
            .next()
            .map(|(key, _)| key.into_owned())
            .unwrap_or_default();
        return bad(&format!("this route takes no query key {key:?}"));
    }
    fresh(StatusCode::OK, &queue_document(&farm))
}

#[cfg(test)]
mod tests;
