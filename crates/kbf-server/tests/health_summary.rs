//! `GET /v1/health` (`docs/design/kbf-ui-v1.md`, N6) over a real listener: every value it
//! reports equals what `/readyz` and the routes it composes report, read in the same
//! state. Each test names the defect it catches.

#[path = "support/api_http.rs"]
mod api_http;
#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use api_http::{get, post};
use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::storage_health::{Debts, Reported};
use kbf_state::Store;
use serde_json::{Value, json};
use support::{Cell, Job, StateHandle};

/// Catches (the route's whole contract: it adds nothing of its own): a `ready` or a
/// `failing` that is not `/readyz`'s (a hard-coded `true`, a failing list from another
/// evaluation), a field of the removed farm (`nodes`, `leases`, `queue`) that comes back, and,
/// with no storage members on this server, a storage object instead of `null`. Read while the
/// server serves, then with the frontend draining and `/readyz` failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_is_what_readyz_reports() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let read = || async {
        let (code, health) = get(api, "/v1/health").await;
        assert_eq!(code, 200, "{health}");
        let (_, readyz) = get(api, "/readyz").await;
        (health, readyz)
    };

    let (health, readyz) = read().await;
    assert_eq!(health["ready"], readyz["ready"], "ready: {health}");
    assert_eq!(health["failing"], readyz["failing"], "failing: {health}");
    assert_eq!(health["ready"], true, "{health}");
    for removed in ["nodes", "leases", "queue"] {
        assert!(health.get(removed).is_none(), "{removed}: {health}");
    }
    assert_eq!(
        health["storage"],
        Value::Null,
        "this server keeps no members"
    );
    assert!(health.get("storage_unavailable").is_none(), "{health}");

    // The frontend drains, with an Execute open so that the API keeps answering: `/readyz`
    // fails with its reason, and so does the summary.
    let job = Job::new("open across the drain", &[]);
    cell.upload(&job.blobs()).await;
    let _open = cell.execute(&job.action).await;
    let (code, _) = post(api, "/v1/frontend/drain", "").await;
    assert_eq!(code, 200);
    let (health, readyz) = read().await;
    assert_eq!(health["ready"], readyz["ready"], "ready: {health}");
    assert_eq!(health["failing"], readyz["failing"], "failing: {health}");
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
    let state = Arc::new(StateHandle {
        store: Arc::clone(&store) as Arc<dyn Store>,
    });
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

/// Wall clock now, in milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970");
    u64::try_from(since.as_millis()).expect("fits")
}

/// Catches: `started_unix_ms` missing, zero, or read from anywhere but the server's start
/// (it must fall between the moments before and after the server started, and not move
/// between two reads); `uptime_ms` missing, a constant, counted from the epoch or from
/// each request (it must grow with the time between two reads, by at least that time,
/// and never exceed the time since the bracket's start).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_when_the_server_started_and_how_long_it_has_run() {
    let before = unix_ms();
    let cell = Cell::start_with_api().await;
    let after = unix_ms();
    let api = cell.api.expect("an API address");

    let (code, first) = get(api, "/v1/health").await;
    assert_eq!(code, 200, "{first}");
    let started = first["started_unix_ms"].as_u64().expect("a number");
    assert!(
        (before..=after).contains(&started),
        "started {started} is outside the start of the server {before}..={after}: {first}"
    );
    let up_first = first["uptime_ms"].as_u64().expect("a number");

    tokio::time::sleep(Duration::from_millis(120)).await;
    let (_, second) = get(api, "/v1/health").await;
    assert_eq!(second["started_unix_ms"], first["started_unix_ms"]);
    let up_second = second["uptime_ms"].as_u64().expect("a number");
    assert!(
        up_second >= up_first + 120,
        "uptime grew from {up_first} to {up_second}, less than the 120 ms slept"
    );
    assert!(
        up_second <= unix_ms() - before,
        "uptime {up_second} is more than the time since before the start"
    );
}
