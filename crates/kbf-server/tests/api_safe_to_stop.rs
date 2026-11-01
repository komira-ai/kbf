//! `GET /v1/members/{id}/safe-to-stop` over a real listener: whether the host of a storage
//! member may be stopped now, from the members a state store holds, behind the same gates as
//! the member write, and without changing anything. The gate's reasons one by one are in the
//! unit tests of `src/api/safe_to_stop.rs`; these tests pin the route: the real members, the
//! gates, the statuses, the daemon answer and a node id judged by the storage member its
//! host enrolled with.

#[path = "support/api_http.rs"]
mod api_http;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_enroll_registry::{EnrollRequest, IssuedRole, TokenSpec, create_token, enroll_txn};
use kbf_front::{Cache, MemoryMetaLog};
use kbf_map::MemberId;
use kbf_objstore::MemoryStore;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::storage_health::{Debts, Reported};
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{NodeId, Role, RoleSet, TokenId, TokenSecret};
use serde_json::{Value, json};
use support::{API_TOKEN, Cell, FakeDaemon, HELLO_WAIT, INTERVAL, hello, hello_on};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The directory the token file is written in.
const DIR: &str = "kbf-server-api-safe-stop";

/// A day, in the store clock's seconds.
const DAY: u64 = 24 * 3600;

/// The headers of an operator call: the token and a JSON content type.
fn operator_headers() -> String {
    format!("Authorization: Bearer {API_TOKEN}\r\nContent-Type: application/json\r\n")
}

/// A tick that does not come within a test: no test here waits for one.
const TICK: Duration = Duration::from_secs(3600);

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
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), DIR);
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
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

struct Server {
    api: SocketAddr,
    /// Where daemons open their sessions.
    worker: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// A server with the operator API and a token, over `state` if given.
fn start(state: Option<StateStore>) -> Server {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(token()),
        local_cache: None,
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
    let worker = bound.worker;
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
        worker,
        stop: Some(stop),
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

/// A storage member and the boot its heartbeats name.
#[derive(Clone, Copy)]
struct Node {
    id: MemberId,
    boot: u64,
}

/// A server over a store with three `Active` members that have each reported a repair debt
/// of zero, and another frontend over the same store to move them with.
fn three_members() -> (Server, Members, [Node; 3]) {
    let clock = Arc::new(ManualClock::default());
    three_members_over(&Arc::new(Fake::with_flaw(clock, Flaw::None)))
}

/// [`three_members`] over the state store `fake`, which the test may write rows to as well.
fn three_members_over(fake: &Arc<Fake>) -> (Server, Members, [Node; 3]) {
    let state = StateStore {
        store: Arc::clone(fake) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let other = Members::new(Arc::clone(fake) as Arc<dyn Store>, config());
    let nodes = [1, 2, 3].map(|i| {
        let registered = other.register(&registrant(i)).expect("register");
        Node {
            id: registered.id,
            boot: registered.boot,
        }
    });
    for node in nodes {
        owes(&other, node, 0);
    }
    (start(Some(state)), other, nodes)
}

/// `node` reports a repair debt of `owed` blobs.
fn owes(members: &Members, node: Node, owed: u64) {
    let said = Reported {
        debts: Debts::Known(owed),
        ..Reported::default()
    };
    members
        .heartbeat(node.id, node.boot, Progress::CaughtUp, said, None)
        .expect("a beat");
}

fn path(node: Node) -> String {
    format!("/v1/members/{}/safe-to-stop", node.id.0)
}

/// `GET path` with `headers`; the status code and the body.
async fn get(api: SocketAddr, path: &str, headers: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let raw = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\n{headers}Connection: close\r\n\r\n");
    stream.write_all(raw.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, body.to_owned())
}

/// The verdict of the stop of `node`, as the operator reads it.
async fn ask(server: &Server, node: Node) -> Value {
    let (code, body) = get(server.api, &path(node), &operator_headers()).await;
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).expect("JSON")
}

/// Catches: a verdict that does not follow the members it reads: all healthy is safe with no
/// reason; a peer that owes blobs, then a peer planned down, make the stop of the others
/// unsafe with the gate's own line; the planned-down member itself is safe to stop and says
/// it is planned down; and asking changes nothing (the map's generation moves once, by the
/// plan, and not by the queries). It catches: the gate inverted; the reasons
/// dropped; the planned-down mark left out of the answer; a query that writes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_verdict_follows_the_members() {
    let (server, other, [one, two, three]) = three_members();
    let before = other.current_map().expect("read").expect("a map");

    let healthy = ask(&server, one).await;
    assert_eq!(
        healthy,
        json!({"safe": true, "reasons": [], "planned_down": false})
    );

    owes(&other, two, 5);
    let owing = ask(&server, one).await;
    let line = format!("member {} owes 5 blobs", two.id);
    assert_eq!(owing["safe"], false);
    assert_eq!(owing["reasons"], json!([line]));
    assert_eq!(ask(&server, three).await["safe"], false);
    owes(&other, two, 0);
    assert_eq!(ask(&server, one).await["safe"], true);

    other
        .plan_down(three.id, Duration::from_secs(600))
        .expect("planned down");
    let planned = ask(&server, one).await;
    let line = format!("member {} is planned down", three.id);
    assert_eq!(planned["safe"], false);
    assert_eq!(planned["reasons"], json!([line]));
    assert_eq!(planned["planned_down"], false);
    let itself = ask(&server, three).await;
    assert_eq!(itself["safe"], true);
    assert_eq!(itself["planned_down"], true);

    let after = other.current_map().expect("read").expect("a map");
    let bumped = before.generation() + 1;
    assert_eq!(after.generation(), bumped);
}

/// Catches: the query skipping a gate the member write passes: no token, a wrong token, a
/// browser's `Origin`, or no JSON content type must each be refused, with nothing of the
/// verdict in the answer. It catches: the `gate` call removed from the handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_query_is_refused_without_the_gates() {
    let (server, _other, [one, _, _]) = three_members();
    let wrong = "Authorization: Bearer not-the-token-not-the-token-000000000\r\n\
                 Content-Type: application/json\r\n";
    let operator = operator_headers();
    let origin = format!("{operator}Origin: http://evil.example\r\n");
    let plain = format!("Authorization: Bearer {API_TOKEN}\r\n");
    for (headers, refused) in [
        ("Content-Type: application/json\r\n", 401),
        ("", 401),
        (wrong, 401),
        (origin.as_str(), 403),
        (plain.as_str(), 415),
    ] {
        let (code, body) = get(server.api, &path(one), headers).await;
        assert_eq!(code, refused, "{headers}: {body}");
        assert!(!body.contains("safe"), "{body}");
    }
}

/// Catches: an id that names nothing answered as a verdict: a number that is no member and a
/// name that is no node are 404 and name the id; a server without a state store keeps no
/// members, so a number is 404 there too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_id_that_names_nothing_is_404() {
    let (server, _other, _nodes) = three_members();
    let operator = operator_headers();
    for id in ["9", "rack-1"] {
        let asked = format!("/v1/members/{id}/safe-to-stop");
        let (code, body) = get(server.api, &asked, &operator).await;
        assert_eq!(code, 404, "{id}: {body}");
        assert!(body.contains(id), "{body}");
    }
    let bare = start(None);
    let asked = "/v1/members/1/safe-to-stop";
    let (code, body) = get(bare.api, asked, &operator).await;
    assert_eq!(code, 404, "{body}");
}

/// Catches: a daemon asked about as if it were a storage member that is unknown (404), or a
/// name that is no daemon answered safe: on a server without a state store (which keeps no
/// members) a node that registered with the farm is safe to stop with the line that says
/// only the daemon was judged, a node that did not is 404. It catches: the farm's
/// node lookup inverted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_is_safe_to_stop() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let _daemon = cell.daemon("linux-1", 4, 8).await;

    let known = "/v1/members/linux-1/safe-to-stop";
    let (code, said) = api_http::get(api, known).await;
    assert_eq!(code, 200, "{said}");
    assert_eq!(said["safe"], true);
    assert_eq!(said["planned_down"], false);
    let reasons = said["reasons"].as_array().expect("reasons");
    assert_eq!(reasons.len(), 1, "{said}");
    let line = reasons[0].as_str().unwrap_or_default();
    assert!(line.contains("daemon"));

    let unknown = "/v1/members/linux-2/safe-to-stop";
    let (code, said) = api_http::get(api, unknown).await;
    assert_eq!(code, 404, "{said}");
}

/// The certificate the registry keeps for `role` of the node enrolled with token `seed`.
fn issued_role(role: Role, seed: u8, i: usize, now: u64) -> IssuedRole {
    let mut spki = [0u8; 32];
    spki[0] = seed;
    spki[1] = u8::try_from(i).expect("small");
    IssuedRole {
        role,
        spki_sha256: spki,
        serial: vec![0, seed, spki[1]],
        not_before: now - 300,
        not_after: now + 7 * DAY,
        cert_der: vec![seed, spki[1]],
    }
}

/// The node `name` enrolled in the registry of `store` with `roles`, advertising `advertise`
/// (empty for a node with no storage role), with the token numbered `n`.
fn enroll(store: &dyn Store, n: u8, name: &str, roles: &[Role], advertise: &str) {
    let spec = TokenSpec {
        roles: RoleSet::new(roles.iter().copied()).expect("roles"),
        host_role: None,
        max_uses: 1,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "lab".to_owned(),
        created_by: "tester".to_owned(),
    };
    let token = TokenId::from_bits(u64::from(n));
    let secret = TokenSecret::from_bytes([n; 32]);
    create_token(store, &token, &secret, &spec).expect("token");
    let node: NodeId = name.parse().expect("a node id");
    let csrs: Vec<(Role, &[u8])> = roles.iter().map(|r| (*r, b"csr".as_slice())).collect();
    let now = store.now().as_duration().as_secs();
    let request = EnrollRequest {
        token_id: &token,
        secret: &[n; 32],
        node_id: &node,
        csrs: &csrs,
        advertise,
        zone: "z",
    };
    let mut sign = || {
        let issued = roles.iter().enumerate();
        Ok(issued
            .map(|(i, role)| issued_role(*role, n, i, now))
            .collect())
    };
    enroll_txn(store, &request, &mut sign).expect("enrolled");
}

/// `GET` of the query for `id` with the operator token: the status code and the JSON body.
async fn ask_by(server: &Server, id: &str) -> (u16, Value) {
    api_http::get(server.api, &format!("/v1/members/{id}/safe-to-stop")).await
}

/// Catches: a node id answered by the daemon line when its host has a storage member (the
/// updater that asks by node id would stop a host with a blob on one copy), or answered
/// safe when it cannot be told: a node that enrolled with the address member 1 registered is
/// judged as member 1 (unsafe while member 2 owes blobs, and the same body as asking by
/// member 1); a node enrolled with no storage address, and a Mac, are safe with the daemon
/// line; a Linux node whose address no member registered, and one the registry does not know,
/// are `409` with why; a node that never registered is `404`. It catches: the node
/// answered as a daemon without asking the registry; an unlinked node answered safe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_is_judged_as_the_storage_member_it_enrolled_with() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(10 * DAY));
    let fake = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let (server, other, [one, two, _]) = three_members_over(&fake);
    let both = [Role::Storage, Role::Daemon];
    enroll(&*fake, 1, "linux-1", &both, "192.0.2.1:8982");
    enroll(&*fake, 2, "linux-2", &[Role::Daemon], "");
    enroll(&*fake, 3, "linux-3", &both, "192.0.2.9:8982");
    let mac = [("arch", "arm64"), ("os", "macos")];
    let hellos = [
        hello("linux-1", 4, 8),
        hello("linux-2", 4, 8),
        hello("linux-3", 4, 8),
        hello("linux-5", 4, 8),
        hello_on("mac-1", 4, 8, &mac),
    ];
    let mut daemons = Vec::new();
    for greeting in hellos {
        let daemon = FakeDaemon::connect(server.worker, greeting).await;
        daemons.push(daemon.expect("registered"));
    }

    owes(&other, two, 5);
    let line = format!("member {} owes 5 blobs", two.id);
    let (code, by_node) = ask_by(&server, "linux-1").await;
    assert_eq!(code, 200, "{by_node}");
    assert_eq!(by_node["safe"], false);
    assert_eq!(by_node["reasons"], json!([line]));
    assert_eq!(by_node, ask(&server, one).await);

    for id in ["linux-2", "mac-1"] {
        let (code, said) = ask_by(&server, id).await;
        assert_eq!(code, 200, "{id}: {said}");
        assert_eq!(said["safe"], true, "{id}");
        let reasons = said["reasons"].as_array().expect("reasons");
        assert_eq!(reasons.len(), 1, "{id}: {said}");
    }

    let unsure = [
        ("linux-3", "no storage member"),
        ("linux-5", "not in the enrollment"),
    ];
    for (id, why) in unsure {
        let (code, said) = ask_by(&server, id).await;
        assert_eq!(code, 409, "{id}: {said}");
        let text = said["error"].as_str().unwrap_or_default();
        assert!(text.contains(why), "{id}: {text}");
    }

    let (code, said) = ask_by(&server, "linux-4").await;
    assert_eq!(code, 404, "{said}");
}
