//! `GET /v1/health` (`docs/design/kbf-ui-v1.md`, N6) over a real listener: every value it
//! reports equals what `/readyz` and the routes it composes report, read in the same
//! state. Each test names the defect it catches.

#[path = "support/api_http.rs"]
mod api_http;
#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use api_http::{get, post};
use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::storage_health::{Debts, Reported};
use kbf_server::{EpochPolicy, StateStore};
use kbf_state::Store;
use serde_json::{Value, json};
use support::{Cell, Job};

/// The six states of `nodes`, counted from the list of `GET /v1/nodes` the way the route
/// says: a node once, in the first of disconnected, quarantined, then its placement.
fn counted(list: &Value) -> Value {
    let mut counts = [0_u64; 6];
    let nodes = list["nodes"].as_array().expect("nodes");
    for node in nodes {
        let state = if node["connected"] == false {
            5
        } else if node["phase"]["state"] == "quarantined" {
            4
        } else {
            match node["placement"]["state"].as_str().expect("a state") {
                "serving" => 0,
                "cordoned" => 1,
                "draining" | "drain_paused" => 2,
                "drained" => 3,
                other => panic!("a placement state this test does not know: {other}"),
            }
        };
        counts[state] += 1;
    }
    json!({
        "total": nodes.len(), "serving": counts[0], "cordoned": counts[1],
        "draining": counts[2], "drained": counts[3], "quarantined": counts[4],
        "disconnected": counts[5],
    })
}

/// `GET /v1/nodes` until `done` holds.
async fn nodes_until(api: SocketAddr, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..250 {
        let (_, list) = get(api, "/v1/nodes").await;
        if done(&list) {
            return list;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the node list never reached the state");
}

/// Catches (the route's whole contract: it adds nothing of its own): a `ready` or a
/// `failing` that is not `/readyz`'s (a hard-coded `true`, a failing list from another
/// evaluation), node states counted other than the route says (a cordoned node counted
/// as draining, a disconnected node still counted by its placement, a node counted
/// twice or not at all), leases counted from anywhere but the nodes' own, a queue depth
/// that is not the one `GET /v1/frontend/drain` reports, and, with no storage members
/// on this server, a storage object instead of `null`. Read while a node serves, then
/// with one leased and draining, one cordoned, one drained, one disconnected and work
/// queued, then with the frontend draining and `/readyz` failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_is_what_readyz_and_the_source_routes_report() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let compare = |health: &Value, list: &Value, drain: &Value, readyz: &Value| {
        assert_eq!(health["ready"], readyz["ready"], "ready: {health}");
        assert_eq!(health["failing"], readyz["failing"], "failing: {health}");
        assert_eq!(health["nodes"], counted(list), "nodes: {health} {list}");
        let leases: u64 = list["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .map(|n| n["lease_count"].as_u64().expect("a count"))
            .sum();
        assert_eq!(health["leases"], leases, "leases: {health}");
        assert_eq!(
            health["queue"]["depth"], drain["queued_operations"],
            "queue: {health}"
        );
        // Null exactly when nothing waits; a number otherwise.
        assert_eq!(
            health["queue"]["oldest_age_ms"].is_null(),
            health["queue"]["depth"] == 0,
            "oldest_age_ms is null only when nothing waits: {health}"
        );
    };
    let read = || async {
        let (code, health) = get(api, "/v1/health").await;
        assert_eq!(code, 200, "{health}");
        let (_, list) = get(api, "/v1/nodes").await;
        let (_, drain) = get(api, "/v1/frontend/drain").await;
        let (_, readyz) = get(api, "/readyz").await;
        (health, list, drain, readyz)
    };

    // One node, serving, nothing queued.
    let mut leased = cell.daemon("n1", 8, 16).await;
    let (health, list, drain, readyz) = read().await;
    compare(&health, &list, &drain, &readyz);
    assert_eq!(health["ready"], true);
    assert_eq!(health["nodes"]["serving"], 1);
    assert_eq!(health["nodes"]["total"], 1);
    assert_eq!(
        health["storage"],
        Value::Null,
        "this server keeps no members"
    );
    assert!(health.get("storage_unavailable").is_none(), "{health}");

    // n1 takes a lease and is drained (so draining); n2 is cordoned; n3 drained; n4 gone.
    let (first, second) = (Job::new("first", &[]), Job::new("second", &[]));
    for job in [&first, &second] {
        cell.upload(&job.blobs()).await;
    }
    let _first = cell.execute(&first.action).await;
    leased.start().await;
    let (_n2, _n3, n4) = (
        cell.daemon("n2", 8, 16).await,
        cell.daemon("n3", 8, 16).await,
        cell.daemon("n4", 8, 16).await,
    );
    for (node, verb) in [
        ("n1", "drain"),
        ("n2", "cordon"),
        ("n3", "drain"),
        ("n4", "cordon"),
    ] {
        let (code, view) = post(api, &format!("/v1/nodes/{node}:{verb}"), "").await;
        assert_eq!(code, 200, "{node}:{verb}: {view}");
    }
    n4.close();
    nodes_until(api, |list| {
        list["nodes"][3]["connected"] == false
            && list["nodes"][2]["placement"]["state"] == "drained"
    })
    .await;
    let _second = cell.execute(&second.action).await;
    let (health, list, drain, readyz) = read().await;
    compare(&health, &list, &drain, &readyz);
    assert_eq!(
        health["nodes"],
        json!({"total": 4, "serving": 0, "cordoned": 1, "draining": 1, "drained": 1,
               "quarantined": 0, "disconnected": 1}),
        "n2 cordoned, n1 draining, n3 drained, n4 gone"
    );
    assert_eq!(health["leases"], 1);
    assert_eq!(
        health["queue"]["depth"], 1,
        "the second job waits: {health}"
    );
    // The age is the scheduler's, the one `GET /v1/queue` shows: bracketed by two reads
    // of that route around the health read (the oldest operation is the same one).
    // Wait until that route shows the job has waited a while (the farm's clock is not
    // the test's), so a health age of zero or a constant cannot pass.
    let mut before = Value::Null;
    for _ in 0..250 {
        before = get(api, "/v1/queue").await.1;
        let age = before["oldest_age_ms"].as_u64();
        if age.is_some_and(|age| age >= 30) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (health, ..) = read().await;
    let (_, after) = get(api, "/v1/queue").await;
    let (low, mid, high) = (
        before["oldest_age_ms"].as_u64().expect("an age"),
        health["queue"]["oldest_age_ms"].as_u64().expect("an age"),
        after["oldest_age_ms"].as_u64().expect("an age"),
    );
    assert!(low >= 30, "the job never showed a waited age: {before}");
    assert!(low <= mid && mid <= high, "{low} <= {mid} <= {high}");

    // The frontend drains: `/readyz` fails with its reason, and so does the summary.
    let (code, _) = post(api, "/v1/frontend/drain", "").await;
    assert_eq!(code, 200);
    let (health, list, drain, readyz) = read().await;
    compare(&health, &list, &drain, &readyz);
    assert_eq!(health["ready"], false);
    assert_eq!(health["failing"][0]["check"], "stopping", "{health}");
}

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(600),
        dwell: Duration::from_secs(1200),
        replication: 2,
    }
}

fn registrant(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// Registers `debts.len()` members, promotes them and has each report its debts.
fn farm(members: &Members, debts: &[Debts]) {
    for (i, debts) in (1_u32..).zip(debts) {
        let joined = members.register(&registrant(i)).expect("registers");
        members
            .set_state(joined.id, MemberState::Active)
            .expect("promoted");
        members
            .heartbeat(
                joined.id,
                joined.boot,
                Progress::CaughtUp,
                Reported::debts(*debts),
                None,
            )
            .expect("beats");
    }
}

/// The three numbers of `GET /v1/storage` that the summary repeats.
fn storage_numbers(storage: &Value) -> Value {
    json!({
        "members_active": storage["members_active"],
        "members_total": storage["members_total"],
        "under_replicated_blobs": storage["under_replicated_blobs"],
    })
}

/// Catches: storage numbers that are not `GET /v1/storage`'s (active and total swapped,
/// the debts summed again, an unknown count shown as zero), and a membership that cannot
/// be read taking the summary down or being read as healthy: a store that fails, and one
/// that panics inside its blocking task, are each `storage: null` with
/// `storage_unavailable` naming the failure, while `ready` and `nodes` still answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_is_the_storage_routes_numbers_or_null_and_why() {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let cell = Cell::start_with_state(state).await;
    let api = cell.api.expect("an API address");
    let members = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());

    // Nobody registered: the count is unknown, in both.
    let (_, health) = get(api, "/v1/health").await;
    let (_, storage) = get(api, "/v1/storage").await;
    assert_eq!(health["storage"], storage_numbers(&storage), "{health}");
    assert_eq!(health["storage"]["under_replicated_blobs"], Value::Null);

    // Two members, one owing 3: known, and 3.
    farm(&members, &[Debts::Known(0), Debts::Known(3)]);
    let (_, health) = get(api, "/v1/health").await;
    let (_, storage) = get(api, "/v1/storage").await;
    assert_eq!(health["storage"], storage_numbers(&storage), "{health}");
    assert_eq!(
        health["storage"],
        json!({"members_active": 2, "members_total": 2, "under_replicated_blobs": 3})
    );

    // One draining: not active, and the count unknown again.
    members
        .set_state(MemberId(1), MemberState::Draining)
        .expect("draining");
    let (_, health) = get(api, "/v1/health").await;
    let (_, storage) = get(api, "/v1/storage").await;
    assert_eq!(health["storage"], storage_numbers(&storage), "{health}");
    assert_eq!(
        health["storage"],
        json!({"members_active": 1, "members_total": 2, "under_replicated_blobs": null})
    );

    // A store that fails, then one that panics: null and why. Another task may read the
    // map first and spend the arming, so each is armed again until it lands.
    let hits = |why: &'static str, arm: &'static dyn Fn(&Hooked)| {
        let (store,) = (Arc::clone(&store),);
        async move {
            for _ in 0..200 {
                arm(&store);
                let (code, health) = get(api, "/v1/health").await;
                assert_eq!(code, 200, "{health}");
                if let Some(found) = health["storage_unavailable"].as_str() {
                    assert!(found.contains(why), "{found}");
                    assert_eq!(health["storage"], Value::Null, "{health}");
                    assert_eq!(health["ready"], true, "the rest still answers: {health}");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the arming never landed in a request");
        }
    };
    hits("injected", &|s| s.fail_gets.store(1, Ordering::SeqCst)).await;
    store.fail_gets.store(0, Ordering::SeqCst);
    hits("storage health task", &|s| {
        s.panic_gets.store(1, Ordering::SeqCst)
    })
    .await;
}
