//! What the server keeps of each host's `UpdateStatus` and serves: the open per-node
//! summary on `GET /v1/nodes`, and the per-host detail on `GET /v1/update/hosts` under
//! `farm.getAuthorizer` (`docs/design/pull-updates.md` 6.3 and 6.4, `update-ui.md` A5),
//! over real listeners, real daemon streams and the `update/*` rows of a state store.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::update::UpdateStatus;
use kbf_proto::update::update_status::State;
use kbf_proto::worker::daemon_message;
use kbf_server::members::MembershipConfig;
use kbf_server::update::{Policy as UpdatePolicy, Release, Rows};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::FarmTime;
use serde_json::{Value, json};
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, hello};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

struct Running {
    api: SocketAddr,
    worker: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn state_over(store: Arc<dyn Store>) -> StateStore {
    StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
    }
}

/// A state store the server is given, and the same store for the test to write rows into.
fn state_and_store() -> (StateStore, Arc<dyn Store>) {
    let clock = Arc::new(ManualClock::default());
    let store = Arc::new(Fake::with_flaw(clock, Flaw::None)) as Arc<dyn Store>;
    (state_over(Arc::clone(&store)), store)
}

/// A policy file's JSON, for `farm.getAuthorizer`.
fn policy(farm: Option<Value>, authentication: Value) -> kbf_auth::Policy {
    let mut file = json!({ "authenticationPolicy": authentication });
    if let Some(farm) = farm {
        file["farm"] = json!({ "getAuthorizer": farm });
    }
    kbf_auth::Policy::from_json(&file.to_string()).expect("a policy")
}

fn farm_open() -> kbf_auth::Policy {
    policy(Some(json!({"allow": {}})), json!({"allow": {}}))
}

fn start(policy: kbf_auth::Policy, state: Option<StateStore>) -> Running {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
        releases: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        policy,
        state,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let (api, worker) = (bound.api.expect("an API address"), bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Running {
        api,
        worker,
        stop: Some(stop),
    }
}

/// `GET path`: the status and the JSON body (`null` for none).
async fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text[9..12].parse().expect("a status");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn update(running: &str, state: State) -> daemon_message::Message {
    daemon_message::Message::UpdateStatus(UpdateStatus {
        running_commit: running.to_owned(),
        state: state as i32,
        ..UpdateStatus::default()
    })
}

/// Polls `GET /v1/nodes` until `done` accepts it.
async fn nodes_until(api: SocketAddr, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (code, nodes) = get(api, "/v1/nodes").await;
        assert_eq!(code, 200, "{nodes}");
        if done(&nodes) {
            return nodes;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never reached: {nodes}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The `update` object of `node` in a `/v1/nodes` answer.
fn summary<'a>(nodes: &'a Value, node: &str) -> &'a Value {
    nodes["nodes"]
        .as_array()
        .and_then(|all| all.iter().find(|n| n["node_id"] == node))
        .map_or(&Value::Null, |n| &n["update"])
}

fn offer(store: &Arc<dyn Store>, commit: &str) {
    Rows::new(Arc::clone(store))
        .offer(&Release::new(commit), FarmTime::from_millis(1))
        .expect("offer");
}

/// Catches the open summary and the tri-state judged wrong. Five hosts and a desired
/// commit: one runs it (`true`), one runs another (`false`), one reports a running commit
/// that is no commit (`null`), one never sent an `UpdateStatus` (`unsupported`, state `null`,
/// never idle) and one sent one and whose stream then closed (`unavailable`: its last words
/// are not live). With no desired commit every verdict is `null`. The summary has exactly
/// three keys.
///
/// Mutants seen red: the verdict of a host that is not `ok` kept; `unknown` read as a
/// mismatch (`false`); a silent host defaulted to `idle`; no desired commit read as a
/// mismatch; a fourth key (`refusal`) on `/v1/nodes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_nodes_update_summary_is_judged_against_the_desired_commit() {
    let (state, rows) = state_and_store();
    let running = start(farm_open(), Some(state));
    let current = FakeDaemon::connect(running.worker, hello("current", 1, 1))
        .await
        .expect("registered");
    current.send(update(&A[..12], State::Idle));
    let behind = FakeDaemon::connect(running.worker, hello("behind", 1, 1))
        .await
        .expect("registered");
    behind.send(update(&B[..12], State::Fetching));
    let unknown = FakeDaemon::connect(running.worker, hello("unknown", 1, 1))
        .await
        .expect("registered");
    unknown.send(update("unknown", State::Idle));
    let _silent = FakeDaemon::connect(running.worker, hello("silent", 1, 1))
        .await
        .expect("registered");
    let gone = FakeDaemon::connect(running.worker, hello("gone", 1, 1))
        .await
        .expect("registered");
    gone.send(update(&A[..12], State::Confirmed));

    let reported = |nodes: &Value| {
        ["current", "behind", "unknown", "gone"]
            .iter()
            .all(|n| summary(nodes, n)["availability"] != "unsupported")
    };
    nodes_until(running.api, reported).await;
    gone.close();
    let nodes = nodes_until(running.api, |n| {
        summary(n, "gone")["availability"] == "unavailable"
    })
    .await;
    for node in ["current", "behind", "unknown", "silent", "gone"] {
        assert_eq!(summary(&nodes, node)["up_to_date"], Value::Null, "{node}");
    }

    offer(&rows, A);
    let nodes = nodes_until(running.api, |n| summary(n, "current")["up_to_date"] == true).await;
    let want = [
        (
            "current",
            json!({"state": "idle", "up_to_date": true, "availability": "ok"}),
        ),
        (
            "behind",
            json!({"state": "fetching", "up_to_date": false, "availability": "ok"}),
        ),
        (
            "unknown",
            json!({"state": "idle", "up_to_date": null, "availability": "ok"}),
        ),
        (
            "silent",
            json!({"state": null, "up_to_date": null, "availability": "unsupported"}),
        ),
        (
            "gone",
            json!({"state": null, "up_to_date": null, "availability": "unavailable"}),
        ),
    ];
    for (node, summary_now) in want {
        assert_eq!(summary(&nodes, node), &summary_now, "{node}");
    }
}

/// Catches an update status that is kept across a restart of the host's daemon as if it
/// were live: a host whose new stream has registered but not yet sent its own is
/// `unavailable` (its previous process's commit is not what it runs), and `ok` again once
/// the new stream reports. Mutant: the register does not clear liveness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_reconnects_is_unavailable_until_its_new_stream_reports() {
    let running = start(farm_open(), None);
    let first = FakeDaemon::connect(running.worker, hello("node-a", 1, 1))
        .await
        .expect("registered");
    first.send(update(&A[..12], State::Idle));
    nodes_until(running.api, |n| {
        summary(n, "node-a")["availability"] == "ok"
    })
    .await;
    let second = FakeDaemon::connect(running.worker, hello("node-a", 1, 1))
        .await
        .expect("registered");
    let nodes = nodes_until(running.api, |n| {
        summary(n, "node-a")["availability"] == "unavailable"
    })
    .await;
    assert_eq!(summary(&nodes, "node-a")["state"], Value::Null);
    second.send(update(&B[..12], State::Confirmed));
    let nodes = nodes_until(running.api, |n| {
        summary(n, "node-a")["availability"] == "ok"
    })
    .await;
    assert_eq!(summary(&nodes, "node-a")["state"], "confirmed");
    drop(first);
}

/// Catches `GET /v1/update/hosts` served to someone who is not allowed: with no policy
/// naming `farm.getAuthorizer` (the default, and no policy file) it is a 404 that says
/// it is off, a `farm` authorizer that denies is 403, an authenticator that refuses is
/// refused before the authorizer is asked, and only an allowing policy answers. The open
/// `/v1/nodes` answers in every case. Mutants seen red: `None` read as allow; the authorizer
/// not asked; the authenticator skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_per_host_detail_needs_the_farm_get_authority() {
    let off = start(kbf_auth::Policy::allow_all(), None);
    let (code, body) = get(off.api, "/v1/update/hosts").await;
    assert_eq!((code, &body["off"]), (404, &json!("farm")), "{body}");
    assert_eq!(get(off.api, "/v1/nodes").await.0, 200);

    let deny = start(
        policy(Some(json!({"deny": {}})), json!({"allow": {}})),
        None,
    );
    let (code, body) = get(deny.api, "/v1/update/hosts").await;
    assert_eq!(code, 403, "{body}");
    assert!(body.get("hosts").is_none(), "{body}");
    assert_eq!(get(deny.api, "/v1/nodes").await.0, 200);

    let unauthenticated = start(
        policy(Some(json!({"allow": {}})), json!({"deny": "no entry"})),
        None,
    );
    let (code, body) = get(unauthenticated.api, "/v1/update/hosts").await;
    assert!(matches!(code, 401 | 403), "{code} {body}");
    assert!(body.get("hosts").is_none(), "{body}");

    let open = start(farm_open(), None);
    let (code, body) = get(open.api, "/v1/update/hosts").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["hosts"], json!([]));
    assert_eq!(body["next_page_token"], Value::Null);
}

/// Catches the detail that leaks into the open route, drops a field, or trusts a host's
/// text. A host sends a 100 KiB refusal of three-byte characters with a newline in it:
/// `/v1/update/hosts` serves at most 512 bytes of it, cut between characters, newline
/// replaced, and the open `/v1/nodes` carries none of it and none of the commits. Also the
/// commits, the generation and the helper flag as sent, and the desired commit.
///
/// Mutants seen red: refusal stored uncut; cut by `chars().take(512)`; control characters
/// kept; refusal or commits on `/v1/nodes`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_detail_names_commits_and_a_refusal_that_is_capped_data() {
    let (state, rows) = state_and_store();
    let running = start(farm_open(), Some(state));
    offer(&rows, A);
    let daemon = FakeDaemon::connect(running.worker, hello("node-a", 1, 1))
        .await
        .expect("registered");
    let refusal = format!("bad\n{}", "\u{20ac}".repeat(100 * 1024 / 3));
    daemon.send(daemon_message::Message::UpdateStatus(UpdateStatus {
        running_commit: B[..12].to_owned(),
        installed_commit: B.to_owned(),
        staged_commit: A.to_owned(),
        state: State::Refused as i32,
        refusal,
        offered_generation: 3,
        helper_present: true,
        ..UpdateStatus::default()
    }));
    let nodes = nodes_until(running.api, |n| summary(n, "node-a")["state"] == "refused").await;
    assert_eq!(
        summary(&nodes, "node-a"),
        &json!({"state": "refused", "up_to_date": false, "availability": "ok"})
    );
    let open = nodes.to_string();
    for secret in [B, A, "bad", "\u{20ac}"] {
        assert!(!open.contains(secret), "/v1/nodes carries {secret:?}");
    }

    let (code, body) = get(running.api, "/v1/update/hosts").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["desired"]["commit"], A);
    let host = &body["hosts"][0];
    assert_eq!(host["node_id"], "node-a");
    assert_eq!(host["availability"], "ok");
    assert_eq!(host["state"], "refused");
    assert_eq!(host["up_to_date"], false);
    assert_eq!(host["desired_commit"], A);
    assert_eq!(host["running_commit"], &B[..12]);
    assert_eq!(host["installed_commit"], B);
    assert_eq!(host["staged_commit"], A);
    assert_eq!(host["offered_generation"], 3);
    assert_eq!(host["helper_present"], true);
    let refusal = host["refusal"].as_str().expect("a refusal");
    assert!(refusal.len() <= 512, "{} bytes", refusal.len());
    assert!(refusal.len() > 500, "{} bytes", refusal.len());
    assert!(refusal.starts_with("bad\u{fffd}"), "{refusal:?}");
    assert!(!refusal.contains('\n'));
}

/// Catches the pause rule on the route: a farm paused through the desired row reads
/// paused when a policy row exists that has no `paused` key (the wrong answer,
/// "running", is a safety error), reads paused with no policy row, running when resumed,
/// and unknown (`null`) before any offer.
///
/// Mutants seen red: `paused` read from the policy row alone (a present policy row
/// without the key reads running); read from the desired row alone is caught by the unit
/// table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_is_reported_by_the_rule_and_never_as_running_by_mistake() {
    let (state, rows_store) = state_and_store();
    let running = start(farm_open(), Some(state));
    let paused = || async {
        let (code, body) = get(running.api, "/v1/update/hosts").await;
        assert_eq!(code, 200, "{body}");
        body["paused"].clone()
    };
    assert_eq!(paused().await, Value::Null, "nothing offered yet");
    let rows = Rows::new(rows_store);
    rows.offer(&Release::new(A), FarmTime::from_millis(1))
        .expect("offer");
    assert_eq!(paused().await, json!(false));
    rows.set_paused(true).expect("pause");
    assert_eq!(paused().await, json!(true), "no policy row");
    rows.set_policy(&UpdatePolicy::default())
        .expect("policy row");
    assert_eq!(paused().await, json!(true), "a policy row without the key");
    rows.set_paused(false).expect("resume");
    assert_eq!(paused().await, json!(false));
}

/// Catches paging that skips or repeats a host: three hosts in pages of two come out as
/// two then one, in node-id order, with a token only where more remain; a bad page size
/// or an unknown query key is 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosts_are_paged_in_node_id_order() {
    let running = start(farm_open(), None);
    let mut daemons = Vec::new();
    for node in ["node-c", "node-a", "node-b"] {
        daemons.push(
            FakeDaemon::connect(running.worker, hello(node, 1, 1))
                .await
                .expect("registered"),
        );
    }
    nodes_until(running.api, |n| {
        n["nodes"].as_array().map(Vec::len) == Some(3)
    })
    .await;
    let ids = |body: &Value| -> Vec<String> {
        body["hosts"]
            .as_array()
            .expect("hosts")
            .iter()
            .map(|h| h["node_id"].as_str().expect("an id").to_owned())
            .collect()
    };
    let (code, first) = get(running.api, "/v1/update/hosts?page_size=2").await;
    assert_eq!(code, 200, "{first}");
    assert_eq!(ids(&first), ["node-a", "node-b"]);
    let token = first["next_page_token"]
        .as_str()
        .expect("a token")
        .to_owned();
    let (_, second) = get(
        running.api,
        &format!("/v1/update/hosts?page_size=2&page_token={token}"),
    )
    .await;
    assert_eq!(ids(&second), ["node-c"]);
    assert_eq!(second["next_page_token"], Value::Null);
    let (_, exact) = get(running.api, "/v1/update/hosts?page_size=3").await;
    assert_eq!(ids(&exact).len(), 3);
    assert_eq!(exact["next_page_token"], Value::Null, "nothing remains");
    for bad in ["page_size=0", "page_size=1001", "page_size=x", "size=2"] {
        let (code, body) = get(running.api, &format!("/v1/update/hosts?{bad}")).await;
        assert_eq!(code, 400, "{bad}: {body}");
    }
}

/// Catches a state store that fails read as "no desired commit": the detail route is a 500
/// (it must not show hosts as unjudgeable and the farm as unpaused), while the open node
/// list is still served, with no verdict, and says nothing false. Healthy again, the
/// verdicts return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_store_that_fails_is_a_500_on_the_detail_and_no_verdict_on_the_list() {
    let hooked = hooked::Hooked::new();
    let running = start(farm_open(), Some(state_over(hooked.clone())));
    Rows::new(hooked.clone())
        .offer(&Release::new(A), FarmTime::from_millis(1))
        .expect("offer");
    let daemon = FakeDaemon::connect(running.worker, hello("node-a", 1, 1))
        .await
        .expect("registered");
    daemon.send(update(&A[..12], State::Idle));
    nodes_until(running.api, |n| summary(n, "node-a")["up_to_date"] == true).await;

    hooked.fail_gets.store(1, Ordering::SeqCst);
    assert_eq!(get(running.api, "/v1/update/hosts").await.0, 500);
    hooked.fail_gets.store(1, Ordering::SeqCst);
    let (code, nodes) = get(running.api, "/v1/nodes").await;
    assert_eq!(code, 200, "{nodes}");
    assert_eq!(summary(&nodes, "node-a")["up_to_date"], Value::Null);
    assert_eq!(summary(&nodes, "node-a")["availability"], "ok");
    // The desired read succeeds and the policy read after it fails.
    let again = Arc::clone(&hooked);
    hooked.after_get("update/desired", move || {
        again.fail_gets.store(1, Ordering::SeqCst);
    });
    assert_eq!(get(running.api, "/v1/update/hosts").await.0, 500);
    nodes_until(running.api, |n| summary(n, "node-a")["up_to_date"] == true).await;
}
