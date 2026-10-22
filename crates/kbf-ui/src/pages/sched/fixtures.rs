//! The documents `GET /v1/leases` and `GET /v1/queue` answer, as the Leases and Queue
//! tests feed them to the pages (`kbf-server`'s `tests/ui_pages_sched.rs` renders the
//! pages over the real routes).

use serde_json::{Value, json};

use crate::tests::{Fake, health_doc};

/// What an attacker-influenced string looks like: markup, both quotes and an ampersand.
pub(crate) const HOSTILE: &str = "<script>alert(1)</script>\"'&";

/// A lease as the API answers it; every number its own value, so one shown under another's
/// column shows.
pub(crate) fn lease(id: &str, node: &str, phase: &str, age_ms: u64, since_ms: u64) -> Value {
    json!({
        "lease": id, "node": node, "phase": phase, "age_ms": age_ms, "since_ms": since_ms,
        "granted_unix_ms": 1_792_000_100_000_u64, "started_unix_ms": null,
        "operation": format!("op-{id}"), "instance": "main",
        "digest": format!("{}/140", "ab".repeat(32)),
        "platform": "arch=x86_64,os=linux", "qos": "ci",
    })
}

/// The link of the page after `/v1/leases?node=node-a&page_size=2`.
pub(crate) const NEXT_OF_NODE_A: &str =
    "/v1/leases?node=node-a&page_size=2&page_token=1792.1.00ff%2Bgg";

/// `GET /v1/leases`: three leases of two nodes in three phases.
pub(crate) fn leases_doc() -> Value {
    json!({"term": "1792000000123456", "next_page": null, "items": [
        lease("1792.0", "node-a", "running", 41_200, 40_100),
        lease("1792.1", "node-a", "granted", 3_000, 2_000),
        lease("1792.2", "node-b", "proposed", 120_500, 61_000),
    ]})
}

/// `GET /v1/leases?node=node-a`, the first of two pages.
pub(crate) fn node_a_page() -> Value {
    json!({"term": "1792000000123456", "next_page": NEXT_OF_NODE_A, "items": [
        lease("1792.0", "node-a", "running", 41_200, 40_100),
        lease("1792.1", "node-a", "granted", 3_000, 2_000),
    ]})
}

/// `GET /v1/queue`: four operations that wait for a Mac nobody has, one kept off a node.
pub(crate) fn queue_doc() -> Value {
    json!({
        "depth": 5, "oldest_age_ms": 61_500,
        "by_platform": [
            {"platform": "os=macos", "qos": "ci", "depth": 4, "oldest_age_ms": 61_500},
            {"platform": "any", "qos": "interactive", "depth": 1, "oldest_age_ms": 900}],
        "waiting": [{
            "reason": "none of the 2 live workers satisfies the platform; node-a lacks os=macos",
            "count": 4, "example_operation": "op-3", "example_instance": "main",
            "example_digest": format!("{}/77", "cd".repeat(32))}],
        "avoided": [{"operation": "op-9", "instance": "main",
            "digest": format!("{}/90", "ef".repeat(32)), "nodes": ["node-b", "node-c"]}],
        "avoided_total": 1,
    })
}

/// A read API that answers the routes of both pages, for the contract test.
pub(crate) fn sched_farm() -> Fake {
    Fake::default()
        .json("/v1/leases", &leases_doc())
        .json("/v1/leases?node=node-a", &node_a_page())
        .json("/v1/queue", &queue_doc())
        .json("/v1/health", &health_doc())
}
