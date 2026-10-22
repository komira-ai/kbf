//! `POST /v1/finalize` and `GET /v1/finalize` over a real listener: the operator asks the farm's
//! storage nodes to raise their metadata logs, through the `Members` the `Membership` service
//! registers into. The checks that keep it from being asked (a member that says nothing of its
//! build, one whose build runs less, one that is down), the statuses, the gates and the
//! report are pinned here; the pure parts are in `src/api/finalize_tests.rs`.

#[path = "support/api_http.rs"]
mod api_http;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_server::members::{
    Fill, Members, MembershipConfig, NodeFacts, PROTOCOL, Progress, Registrant, Registration,
};
use kbf_server::raft_log::RUNS;
use kbf_server::storage_health::Reported;
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use serde_json::{Value, json};
use support::{API_TOKEN, HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A tick that does not come within a test: the update controller reads the store at every
/// tick, and no test here waits for one.
const TICK: Duration = Duration::from_secs(3600);

/// The headers of an operator call: the token and a JSON content type.
fn operator_headers() -> String {
    format!("Authorization: Bearer {API_TOKEN}\r\nContent-Type: application/json\r\n")
}

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

/// The token file is in a directory of its own, removed once the token is read.
fn token() -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "api-finalize");
    let path = dir.join("token");
    std::fs::write(&path, format!("{API_TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: TICK,
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        default_platform: kbf_front::DefaultPlatform::NONE,
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

struct Server {
    api: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// A server with the operator API, whose writes need `token` (none: writes are off), over
/// `state` if given.
fn start(state: Option<StateStore>, token: Option<ApiToken>) -> Server {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token,
        local_cache: None,
        releases: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        state,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
        stop: Some(stop),
    }
}

/// A server over a new store, and a second frontend over the same store: it registers the
/// nodes, reads the key the server writes and moves the clock.
struct Farm {
    server: Server,
    members: Members,
    clock: Arc<ManualClock>,
}

fn farm() -> Farm {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let state = StateStore {
        store: Arc::clone(&fake) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    Farm {
        server: start(Some(state), Some(token())),
        members: Members::new(fake as Arc<dyn Store>, config()),
        clock,
    }
}

/// What a node that runs log version `runs_log` and has its log at `log_version` says.
fn said(runs_log: u32, log_version: u32) -> NodeFacts {
    NodeFacts {
        runs_log: Some(runs_log),
        log_version: Some(log_version),
        build_commit: Some("0123456789abcdef".to_owned()),
    }
}

/// Storage node `i` registers saying `facts`.
fn join(members: &Members, i: u32, facts: &NodeFacts) -> Registration {
    let who = Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("192.0.2.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    };
    members
        .register_reporting(&who, None, facts)
        .expect("registers")
}

/// The member `joined` beats saying `facts`.
fn beat(members: &Members, joined: &Registration, facts: &NodeFacts) {
    members
        .heartbeat_reporting(
            joined.id,
            joined.boot,
            Progress::CaughtUp,
            Reported::default(),
            (Fill::default(), facts),
            None,
        )
        .expect("beats");
}

/// One HTTP/1.1 request with `headers` (each line ended by a CRLF) and `body`: the status and
/// the JSON body (`null` if there is none).
async fn call(
    api: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         {headers}Connection: close\r\n\r\n{body}"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// The members a 409 names, as `(member, state)` pairs in the order the answer lists them.
fn blocked(answer: &Value) -> Vec<(u64, String)> {
    let listed = answer["blocked"].as_array().expect("a list of members");
    listed
        .iter()
        .map(|n| {
            let state = n["state"].as_str().expect("a state").to_owned();
            (n["member"].as_u64().expect("a member"), state)
        })
        .collect()
}

/// Asserts that a finalize with no body asks for the newest version the build runs and writes
/// it when every member can raise: 200 with the target and `written: true`, the key is then
/// at the target for another frontend over the store; the same ask again, and a lower one, are
/// 200 with `written: false` and change nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_raises_the_target_once_and_a_repeat_writes_nothing() {
    let farm = farm();
    join(&farm.members, 1, &said(RUNS.get(), 4));
    join(&farm.members, 2, &said(RUNS.get(), RUNS.get()));
    let target = RUNS.get();

    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", "").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, json!({"target": target, "written": true}));
    assert_eq!(farm.members.finalize_target().expect("read"), Some(target));

    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", "{}").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, json!({"target": target, "written": false}));

    let lower = json!({"target": target - 1}).to_string();
    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", &lower).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, json!({"target": target - 1, "written": false}));
    assert_eq!(farm.members.finalize_target().expect("read"), Some(target));
}

/// Asserts that `GET /v1/finalize` answers a null target and the members judged against the
/// newest version the build runs before anyone asked, and the target and each member's state
/// after: a member whose log is below the target is `pending`, one at it `done`, with what each
/// reported (its build, the newest version it runs, where its log is).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_report_names_each_node_and_its_state_against_the_target() {
    let farm = farm();
    let newest = RUNS.get();
    join(&farm.members, 1, &said(newest, newest - 1));
    join(&farm.members, 2, &said(newest, newest));

    let (code, body) = api_http::get(farm.server.api, "/v1/finalize").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["target"], Value::Null);
    let nodes = body["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 2, "{body}");
    assert_eq!(nodes[0]["state"], "pending");
    assert_eq!(nodes[1]["state"], "done");

    let (code, _) = api_http::post(farm.server.api, "/v1/finalize", "").await;
    assert_eq!(code, 200);
    let (code, body) = api_http::get(farm.server.api, "/v1/finalize").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["target"], newest);
    let first = &body["nodes"][0];
    assert_eq!(first["member"], 1);
    assert_eq!(first["build_commit"], "0123456789abcdef");
    assert_eq!(first["runs_log"], newest);
    assert_eq!(first["log_version"], newest - 1);
    assert_eq!(first["state"], "pending");
    assert_eq!(first["reason"], "");
}

/// Asserts that a member that says nothing of its build or log keeps the finalize from being
/// written: 409 naming it as `unknown`, with a reason, and nothing written, so the key is
/// absent for another frontend and the report still has no target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_that_reports_no_facts_blocks_a_finalize() {
    let farm = farm();
    join(&farm.members, 1, &said(RUNS.get(), 1));
    join(&farm.members, 2, &NodeFacts::default());

    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", "").await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(blocked(&body), [(2, "unknown".to_owned())]);
    assert_eq!(body["target"], RUNS.get());
    let reason = body["blocked"][0]["reason"].as_str().expect("a reason");
    assert!(reason.contains("runs_log"), "{reason}");
    assert_eq!(farm.members.finalize_target().expect("read"), None);
    let (_, report) = api_http::get(farm.server.api, "/v1/finalize").await;
    assert_eq!(report["target"], Value::Null);
}

/// Asserts that a member whose build runs less than the target blocks the finalize: 409 naming
/// it as `blocked` with how much its build runs, while the member that runs the target does not
/// block. A target the old build does run is not blocked by it: asked at the version it runs,
/// the same farm is 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_whose_build_runs_less_than_the_target_blocks_a_finalize() {
    let farm = farm();
    let newest = RUNS.get();
    join(&farm.members, 1, &said(newest, 1));
    join(&farm.members, 2, &said(newest - 1, 1));

    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", "").await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(blocked(&body), [(2, "blocked".to_owned())]);
    let reason = body["blocked"][0]["reason"].as_str().expect("a reason");
    let wanted = format!("up to {}", newest - 1);
    assert!(reason.contains(&wanted), "{reason}");
    assert_eq!(farm.members.finalize_target().expect("read"), None);

    let lower = json!({"target": newest - 1}).to_string();
    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", &lower).await;
    assert_eq!(code, 200, "{body}");
    let held = farm.members.finalize_target().expect("read");
    assert_eq!(held, Some(newest - 1));
}

/// Asserts that a member of the map that is down, with no live row, blocks the finalize: 409
/// naming it as `blocked` for having no live row, with the member that is up not named. The
/// down member is one that registered and fell silent past its time to live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_that_is_down_blocks_a_finalize() {
    let farm = farm();
    let newest = RUNS.get();
    let up = said(newest, newest);
    let first = join(&farm.members, 1, &up);
    join(&farm.members, 2, &up);
    farm.clock.advance(Duration::from_secs(20));
    beat(&farm.members, &first, &up);
    farm.clock.advance(Duration::from_secs(20));
    farm.members.refresh().expect("member 2 goes down");

    let (code, body) = api_http::post(farm.server.api, "/v1/finalize", "").await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(blocked(&body), [(2, "blocked".to_owned())]);
    let reason = body["blocked"][0]["reason"].as_str().expect("a reason");
    assert!(reason.contains("no live row"), "{reason}");
    assert_eq!(farm.members.finalize_target().expect("read"), None);
}

/// Asserts that a target the build cannot run is the caller's mistake, 400, and writes
/// nothing: one past the newest version the build runs, 0, a body that is not JSON and a body
/// with a field that is not `target`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_target_past_the_build_or_a_malformed_body_is_a_bad_request() {
    let farm = farm();
    join(&farm.members, 1, &said(RUNS.get(), 1));
    let past = json!({"target": RUNS.get() + 1}).to_string();
    for body in [past.as_str(), r#"{"target":0}"#, "not json", r#"{"to":3}"#] {
        let (code, answer) = api_http::post(farm.server.api, "/v1/finalize", body).await;
        assert_eq!(code, 400, "{body}: {answer}");
        assert!(answer["error"].is_string(), "{answer}");
    }
    assert_eq!(farm.members.finalize_target().expect("read"), None);
}

/// Asserts that both routes are behind the operator gate: a call with no token is 401, one
/// with the wrong token is 401, one without the JSON content type is 415, one from a browser
/// page (with an `Origin`) is 403, none of them writes the key, and a server started with no
/// token has writes off, 403, for both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_routes_need_the_operator_gate() {
    let farm = farm();
    join(&farm.members, 1, &said(RUNS.get(), 1));
    let json_type = "Content-Type: application/json\r\n";
    let wrong = "Authorization: Bearer not-the-token-not-the-token-not-the-token\r\n";
    let origin = format!("{}Origin: http://example.test\r\n", operator_headers());
    let token_only = format!("Authorization: Bearer {API_TOKEN}\r\n");
    let refusals = [
        (json_type.to_owned(), 401),
        (format!("{wrong}{json_type}"), 401),
        (token_only, 415),
        (origin, 403),
    ];
    for method in ["GET", "POST"] {
        for (headers, want) in &refusals {
            let api = farm.server.api;
            let (code, answer) = call(api, method, "/v1/finalize", headers, "").await;
            assert_eq!(code, *want, "{method} with {headers:?}: {answer}");
        }
    }
    assert_eq!(farm.members.finalize_target().expect("read"), None);

    let operator = operator_headers();
    let (code, _) = call(farm.server.api, "POST", "/v1/finalize", &operator, "").await;
    assert_eq!(code, 200, "the gate lets the operator through");

    let off = start(None, None);
    for method in ["GET", "POST"] {
        let (code, answer) = call(off.api, method, "/v1/finalize", &operator, "").await;
        assert_eq!(code, 403, "{method}: {answer}");
    }
}

/// Asserts that a server with no state store, which keeps no storage members, answers both
/// routes 404 once the call is through the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_no_members_has_nothing_to_finalize() {
    let server = start(None, Some(token()));
    let (code, body) = api_http::get(server.api, "/v1/finalize").await;
    assert_eq!(code, 404, "{body}");
    let (code, body) = api_http::post(server.api, "/v1/finalize", "").await;
    assert_eq!(code, 404, "{body}");
}
