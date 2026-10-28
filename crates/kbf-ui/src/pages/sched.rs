//! What the Leases and Queue pages share: the documents of `GET /v1/leases` and
//! `GET /v1/queue` (`docs/api-scheduler.md`), a human reading of an age, and the
//! link from a node id to that node's leases.
//!
//! The documents are declared here, not in `api.rs`, so that the pages of this unit
//! change no shared file but the registry. Like the rest of `api.rs` they name only the
//! keys a page shows.

use form_urlencoded::Serializer;
use serde::Deserialize;

use super::started::Started;
use crate::html::{Href, Html, attr};

/// `GET /v1/leases`.
#[derive(Debug, Deserialize)]
pub(crate) struct LeasesDoc {
    /// The scheduler's leader term, a string: the API sends a 64-bit number as text.
    pub(crate) term: String,
    pub(crate) items: Vec<Lease>,
    /// The root-relative link of the next page, `null` on the last.
    pub(crate) next_page: Option<String>,
}

/// One lease held.
#[derive(Debug, Deserialize)]
pub(crate) struct Lease {
    pub(crate) lease: String,
    pub(crate) node: String,
    pub(crate) phase: String,
    pub(crate) age_ms: u64,
    pub(crate) since_ms: u64,
    pub(crate) operation: String,
    pub(crate) instance: String,
    pub(crate) digest: String,
    pub(crate) platform: String,
    pub(crate) qos: String,
}

/// `GET /v1/queue`.
#[derive(Debug, Deserialize)]
pub(crate) struct QueueDoc {
    pub(crate) depth: u64,
    pub(crate) oldest_age_ms: Option<u64>,
    pub(crate) by_platform: Vec<Group>,
    pub(crate) waiting: Vec<Waiting>,
    pub(crate) avoided: Vec<Avoided>,
    pub(crate) avoided_total: u64,
    /// Present only when the server runs with `--whole-node-max`.
    #[serde(default)]
    pub(crate) whole_node: Option<WholeNode>,
}

/// The whole-node section of the queue document: the caps' numbers and the whole-machine
/// requests that wait.
#[derive(Debug, Deserialize)]
pub(crate) struct WholeNode {
    pub(crate) held: u64,
    pub(crate) marks: u64,
    pub(crate) draining: u64,
    pub(crate) ceiling: u64,
    pub(crate) max_whole_node: u64,
    pub(crate) capable_hosts: u64,
    pub(crate) keep_for_ordinary: u64,
    pub(crate) max_draining: u64,
    /// Requests by reason, every reason present.
    pub(crate) waiting: std::collections::BTreeMap<String, u64>,
    pub(crate) requests: Vec<WholeRequest>,
    pub(crate) requests_total: u64,
}

/// One queued whole-machine request: `state` and `reason` always, the rest where the
/// state has them.
#[derive(Debug, Deserialize)]
pub(crate) struct WholeRequest {
    pub(crate) operation: String,
    pub(crate) instance: String,
    pub(crate) state: String,
    pub(crate) reason: String,
    pub(crate) may_hold_in_ms: Option<u64>,
    pub(crate) worker: Option<String>,
    pub(crate) held_for_ms: Option<u64>,
    pub(crate) deadline_in_ms: Option<u64>,
    pub(crate) give_ups: Option<u64>,
    pub(crate) draining: Option<bool>,
    pub(crate) barred: Vec<BarredWorker>,
    pub(crate) unservable: Option<String>,
}

/// A worker a request gave up and may not hold until its back-off ends.
#[derive(Debug, Deserialize)]
pub(crate) struct BarredWorker {
    pub(crate) worker: String,
    /// `None` for a bar that has no end.
    pub(crate) until_in_ms: Option<u64>,
}

/// The queued operations of one platform and QoS level.
#[derive(Debug, Deserialize)]
pub(crate) struct Group {
    pub(crate) platform: String,
    pub(crate) qos: String,
    pub(crate) depth: u64,
    pub(crate) oldest_age_ms: u64,
}

/// Operations no live worker can run, for one reason.
#[derive(Debug, Deserialize)]
pub(crate) struct Waiting {
    pub(crate) reason: String,
    pub(crate) count: u64,
    pub(crate) example_operation: String,
    pub(crate) example_instance: String,
    pub(crate) example_digest: String,
}

/// A queued operation that placement keeps off some nodes.
#[derive(Debug, Deserialize)]
pub(crate) struct Avoided {
    pub(crate) operation: String,
    pub(crate) instance: String,
    pub(crate) digest: String,
    pub(crate) nodes: Vec<String>,
}

/// `ms` milliseconds as the two largest units that matter: `850 ms`, `41.2 s`,
/// `3 min 20 s`, `2 h 5 min`, `4 d 3 h`. Truncated, never rounded up into the next unit.
pub(crate) fn age(ms: u64) -> String {
    const SECOND: u64 = 1000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if ms < SECOND {
        format!("{ms} ms")
    } else if ms < MINUTE {
        format!("{}.{} s", ms / SECOND, (ms % SECOND) / 100)
    } else if ms < HOUR {
        format!("{} min {} s", ms / MINUTE, (ms % MINUTE) / SECOND)
    } else if ms < DAY {
        format!("{} h {} min", ms / HOUR, (ms % HOUR) / MINUTE)
    } else {
        format!("{} d {} h", ms / DAY, (ms % DAY) / HOUR)
    }
}

/// The Leases page of one node: `/ui/leases?node={node}`.
pub(crate) fn node_leases(node: &str) -> Href {
    let mut query = Serializer::new(String::new());
    query.append_pair("node", node);
    Href::root_relative(&format!(
        "{}?{}",
        super::leases::LEASES_PATH,
        query.finish()
    ))
    .expect("the Leases path is root-relative and the query is encoded")
}

/// A node id as a link to that node's leases.
pub(crate) fn node_link(h: &mut Html, node: &str) {
    h.link(&node_leases(node), node);
}

/// The banner both pages carry: the scheduler's state is memory only, and when the
/// server started.
pub(crate) fn memory_note(h: &mut Html, started: &Started) {
    h.el("p", &[attr("class", "hint")], |h| {
        h.text(
            "The scheduler holds this in memory: after a restart or a handover it is empty \
             until the daemons reconnect, so an empty page is not by itself an outage.",
        );
    });
    super::started::note(h, started);
}

/// A digest or an id, set in code.
pub(crate) fn code(h: &mut Html, text: &str) {
    h.el("code", &[], |h| {
        h.text(text);
    });
}

#[cfg(test)]
pub(crate) mod fixtures;
