//! `GET /v1/storage` and the storage-health families of `GET /metrics` on the operator API
//! of a server with a state store (`kbf_server::storage_health`), over a real listener:
//! the number and the reasons as JSON (`null` while unknown), the metric present only
//! while it is known, and both answering when the membership cannot be read.
//!
//! Catches: the route or the families not mounted; an unknown number shown as zero in
//! either; and a failing membership taking the scrape down with it.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::storage_health::{Debts, Reported};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::{Cond, Store, Txn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A long time to live, so the server's own expiry timer (which runs on the real clock
/// every third of it) does not wake during a test; the store's clock is the test's.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(600),
        dwell: Duration::from_secs(1200),
        replication: 2,
    }
}

struct Server {
    api: SocketAddr,
    worker: SocketAddr,
    store: Arc<Hooked>,
    members: Members,
}

fn start() -> Server {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        scheduler_core: kbf_server::SchedulerCore::under_test(),
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        usage_profiles: kbf_server::UsageProfiles::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let api = Api {
        listen: loopback(),
        token: None,
        local_cache: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners,
        Some(api),
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    let api = bound.api.expect("an API address");
    let worker = bound.worker;
    tokio::spawn(bound.serving);
    Server {
        api,
        worker,
        members: Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config()),
        store,
    }
}

/// One `GET`: the status code and the body.
async fn get(api: SocketAddr, path: &str) -> (u16, String) {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, body.to_owned())
}

async fn storage(s: &Server) -> (u16, Value) {
    let (code, body) = get(s.api, "/v1/storage").await;
    (code, serde_json::from_str(&body).expect("JSON"))
}

fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// Registers `n` members, promotes them and has each report `debts[i]`.
fn farm(s: &Server, debts: &[Debts]) {
    for (i, debts) in (1_u32..).zip(debts) {
        let joined = s.members.register(&node(i)).expect("registers");
        s.members
            .set_state(joined.id, MemberState::Active)
            .expect("promoted");
        s.members
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

/// Catches: the JSON answering with the wrong shape or a number where there is none. With
/// nobody registered the number is `null` and says why; with two members, one owing 3,
/// it is 3 and the reason names the count; with one member draining it is `null` again
/// and names the member (the lost-copy case), though the debts of the others still show.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_answers_the_number_or_null_and_why() {
    let s = start();
    let (code, empty) = storage(&s).await;
    assert_eq!(code, 200);
    assert_eq!(
        empty,
        json!({
            "members_total": 0, "members_active": 0, "not_active": [], "silent": [],
            "unreported": [], "debts_total": 0, "under_replicated_blobs": null,
            "reasons": ["no storage member is registered"], "nodes": [],
        })
    );

    farm(&s, &[Debts::Known(0), Debts::Known(3)]);
    let (_, owed) = storage(&s).await;
    assert_eq!(
        owed,
        json!({
            "members_total": 2, "members_active": 2, "not_active": [], "silent": [],
            "unreported": [], "debts_total": 3, "under_replicated_blobs": 3,
            "reasons": ["3 blobs owed"],
            "nodes": [
                { "id": 1, "state": "active", "planned_down": false, "repair_debts": 0, "open_writes": null, "generation": 0, "unflushed_bytes": null, "unflushed_blocks": null, "oldest_unflushed_ms": null, "budget_bytes": null, "used_bytes": null, "flush_failures": null, "flush_conflicts": null },
                { "id": 2, "state": "active", "planned_down": false, "repair_debts": 3, "open_writes": null, "generation": 0, "unflushed_bytes": null, "unflushed_blocks": null, "oldest_unflushed_ms": null, "budget_bytes": null, "used_bytes": null, "flush_failures": null, "flush_conflicts": null },
            ],
        })
    );

    s.members
        .set_state(MemberId(1), MemberState::Draining)
        .expect("draining");
    let (_, draining) = storage(&s).await;
    assert_eq!(draining["under_replicated_blobs"], Value::Null);
    assert_eq!(draining["not_active"], json!([1]));
    assert_eq!(draining["debts_total"], 3);
    assert_eq!(draining["reasons"], json!(["member 1 not active"]));

    s.members
        .set_state(MemberId(1), MemberState::Active)
        .expect("active");
    s.members
        .heartbeat(
            MemberId(2),
            1,
            Progress::CaughtUp,
            Reported::debts(Debts::Known(0)),
            None,
        )
        .expect("beats");
    let (_, zero) = storage(&s).await;
    assert_eq!(zero["under_replicated_blobs"], 0);
    assert_eq!(zero["reasons"], json!([]));
}

/// Catches: the metric exported as 0 while unknown, or absent while known. Unknown (no
/// member, then a draining one): `kbf_storage_health_known 0` and no
/// `kbf_under_replicated_blobs` line at all. Known: both, with the number. The families
/// sit beside the cache's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scrape_has_the_number_only_while_it_is_known() {
    let s = start();
    let (code, unknown) = get(s.api, "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        unknown.contains("\nkbf_storage_health_known 0\n"),
        "{unknown}"
    );
    assert!(!unknown.contains("kbf_under_replicated_blobs"), "{unknown}");
    assert!(
        unknown.contains("# TYPE "),
        "the cache's families too: {unknown}"
    );

    farm(&s, &[Debts::Known(2), Debts::Known(0)]);
    let (_, known) = get(s.api, "/metrics").await;
    assert!(
        known.contains("\nkbf_under_replicated_blobs 2\n"),
        "{known}"
    );
    assert!(known.contains("\nkbf_storage_health_known 1\n"), "{known}");

    s.members
        .set_state(MemberId(2), MemberState::Draining)
        .expect("draining");
    let (_, drained) = get(s.api, "/metrics").await;
    assert!(!drained.contains("kbf_under_replicated_blobs"), "{drained}");
    assert!(
        drained.contains("\nkbf_storage_health_known 0\n"),
        "{drained}"
    );
}

/// Arms a panic in the store's next read of the map and asks `ask`; asks again, re-armed, until
/// `blown` says the panic landed in the request. Another task may read the map first (a map
/// push, after the farm changed it), and then the hook is spent on it and the request is
/// answered from a healthy store.
async fn blown<T, F: std::future::Future<Output = T>>(
    store: &Hooked,
    ask: impl Fn() -> F,
    blown: impl Fn(&T) -> bool,
) -> T {
    for _ in 0..200 {
        store.after_get("map", || panic!("the store blew up"));
        let answer = ask().await;
        if blown(&answer) {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the store's panic never landed in a request");
}

/// Catches: a membership that cannot be read taking the API or the scrape down, or being
/// read as a number. A panic in the store's read answers 503 with the reason on
/// `/v1/storage`, and the scrape still answers 200 with the known flag at 0 and no number;
/// a stored map that does not decode is a 503 too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_membership_that_cannot_be_read_is_a_503_and_an_unknown_scrape() {
    let s = start();
    farm(&s, &[Debts::Known(0)]);
    let (code, _) = storage(&s).await;
    assert_eq!(
        code, 200,
        "warmed up: the server's own first timer tick is over"
    );

    let (code, body) = blown(&s.store, || storage(&s), |(code, _)| *code == 503).await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("storage health task")),
        "{body}"
    );

    let (code, scrape) = blown(
        &s.store,
        || get(s.api, "/metrics"),
        |(_, text)| text.contains("\nkbf_storage_health_known 0\n"),
    )
    .await;
    assert_eq!(code, 200);
    assert!(
        scrape.contains("\nkbf_storage_health_known 0\n"),
        "{scrape}"
    );
    assert!(!scrape.contains("kbf_under_replicated_blobs"), "{scrape}");

    // A stored map that does not decode is an error, not a panic.
    s.store
        .fake
        .txn(Txn::new().put("map", vec![0xff], Cond::Any))
        .expect("store")
        .expect("commit");
    let (code, body) = storage(&s).await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("unreadable")),
        "{body}"
    );
}

const VITALS: [&str; 7] = [
    "unflushed_bytes",
    "unflushed_blocks",
    "oldest_unflushed_ms",
    "budget_bytes",
    "used_bytes",
    "flush_failures",
    "flush_conflicts",
];

/// The seven storage-health values of node `index` in `GET /v1/storage`, in [`VITALS`] order.
async fn vitals_of(s: &Server, index: usize) -> Vec<Value> {
    let (_, body) = storage(s).await;
    VITALS
        .iter()
        .map(|name| body["nodes"][index][name].clone())
        .collect()
}

/// Catches, over the real membership service and the real route: a heartbeat's write-tier
/// health lost between the wire and the JSON (a field not stored, or read from another
/// field number); an unknown value shown as 0 (a node that sends nothing, a registration
/// that clears the report, and a node that reports a real 0 must differ: `null` against
/// `0`); an absurd value (above 2^53 - 1, which JSON cannot carry exactly) shown instead of
/// dropped; and values that do not come back after the node registers again and beats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_heartbeats_storage_health_reaches_the_api_and_null_is_unknown() {
    use kbf_proto::kbf::map::v1 as pb;
    use kbf_proto::kbf::map::v1::membership_client::MembershipClient;

    let s = start();
    let mut client = MembershipClient::connect(format!("http://{}", s.worker))
        .await
        .expect("connects");
    let register = pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: "node-1".to_owned(),
        addr: "192.0.2.1:8982".to_owned(),
        weight: 1,
        protocol: PROTOCOL,
        ..pb::RegisterRequest::default()
    };
    let beat = |joined: &pb::RegisterResponse| pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot: joined.boot,
        ..pb::HeartbeatRequest::default()
    };
    let unknown = vec![Value::Null; 7];

    let joined = client.register(register.clone()).await.expect("registers");
    let joined = joined.into_inner();
    // An older node: a beat with none of the fields.
    client.heartbeat(beat(&joined)).await.expect("beats");
    assert_eq!(vitals_of(&s, 0).await, unknown, "an old node says nothing");

    let said = pb::HeartbeatRequest {
        unflushed_bytes: Some(11),
        unflushed_blocks: Some(0),
        oldest_unflushed_ms: Some(33),
        budget_bytes: Some(44),
        used_bytes: Some(55),
        flush_failures: Some(66),
        // Not a count of anything: dropped, not shown.
        flush_conflicts: Some(u64::MAX),
        ..beat(&joined)
    };
    client.heartbeat(said.clone()).await.expect("beats");
    let shown = [11, 0, 33, 44, 55, 66].map(|v| json!(v));
    let mut want = shown.to_vec();
    want.push(Value::Null);
    assert_eq!(vitals_of(&s, 0).await, want, "a real 0 stays 0");

    // The node registers again (a restart): the new boot has said nothing yet.
    let again = client
        .register(register)
        .await
        .expect("registers again")
        .into_inner();
    assert_ne!(again.boot, joined.boot, "a new boot");
    assert_eq!(vitals_of(&s, 0).await, unknown, "nothing carried over");
    client
        .heartbeat(pb::HeartbeatRequest {
            member_id: again.member_id,
            boot: again.boot,
            ..said
        })
        .await
        .expect("beats");
    assert_eq!(vitals_of(&s, 0).await, want, "values recover");
}
