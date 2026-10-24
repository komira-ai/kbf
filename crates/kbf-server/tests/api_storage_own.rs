//! `GET /v1/nodes/{id}/storage-health` over a real listener: a storage host's own member and the
//! two storage-wide numbers, from the members a state store holds and the addresses nodes
//! enrolled with. The join and the fields one by one are in the unit tests of
//! `src/api/storage_own.rs`; these tests pin the route: the real members and registry, what the
//! answer holds and leaves out, every way a node can stand, and that asking changes nothing.

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
use support::{API_TOKEN, HELLO_WAIT, INTERVAL};

/// The directory the token file is written in.
const DIR: &str = "kbf-server-api-storage-own";

/// A day, in the store clock's seconds.
const DAY: u64 = 24 * 3600;

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
        memory_ladder: kbf_server::MemoryLadder::default(),
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
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
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
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

/// The node's own view, with the operator token: the status code and the JSON body.
async fn ask(server: &Server, id: &str) -> (u16, Value) {
    api_http::get(server.api, &format!("/v1/nodes/{id}/storage-health")).await
}

/// A node enrolled with the address member 1 registered is shown that member, active, and the
/// two storage numbers (all three members active, nothing owed, then five owed by member 2);
/// the answer names no other member and no address, and asking changes no member or map.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_is_shown_its_own_member_and_the_storage_numbers_only() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(10 * DAY));
    let fake = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let (server, other, [one, two, _]) = three_members_over(&fake);
    enroll(
        &*fake,
        1,
        "linux-1",
        &[Role::Storage, Role::Daemon],
        "192.0.2.1:8982",
    );
    let before = other.current_map().expect("read").expect("a map");

    let (code, said) = ask(&server, "linux-1").await;
    assert_eq!(code, 200, "{said}");
    assert_eq!(
        said,
        json!({
            "node": "linux-1",
            "half": "member",
            "member": {"id": one.id.0, "state": "active", "build_commit": null},
            "storage": {"members_active": 3, "under_replicated_blobs": 0},
        })
    );
    let text = said.to_string();
    for other_member in ["192.0.2", "linux-2"] {
        assert!(!text.contains(other_member), "{text}");
    }

    owes(&other, two, 5);
    let (_, said) = ask(&server, "linux-1").await;
    assert_eq!(said["storage"]["under_replicated_blobs"], 5, "{said}");
    assert_eq!(said["member"]["id"], one.id.0);
    owes(&other, two, 0);

    let after = other.current_map().expect("read").expect("a map");
    assert_eq!(after.generation(), before.generation());
}

/// Each way a node can stand, with no member in the answer: enrolled with no storage address,
/// enrolled with an address no member registered, and not in the registry; a server with no
/// state store keeps no members and answers 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_without_a_member_is_told_how_it_stands() {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(10 * DAY));
    let fake = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let (server, _other, _nodes) = three_members_over(&fake);
    enroll(&*fake, 2, "linux-2", &[Role::Daemon], "");
    enroll(
        &*fake,
        3,
        "linux-3",
        &[Role::Storage, Role::Daemon],
        "192.0.2.9:8982",
    );

    for (id, half, why) in [
        ("linux-2", "no_storage", None),
        ("linux-3", "unlinked", Some("no storage member registered")),
        (
            "linux-9",
            "unknown_node",
            Some("not in the enrollment registry"),
        ),
    ] {
        let (code, said) = ask(&server, id).await;
        assert_eq!(code, 200, "{id}: {said}");
        assert_eq!(said["half"], half, "{id}");
        assert!(said["member"].is_null(), "{id}: {said}");
        assert_eq!(said["storage"]["members_active"], 3, "{id}");
        match why {
            None => assert!(said.get("why").is_none(), "{id}: {said}"),
            Some(text) => {
                let said_why = said["why"].as_str().unwrap_or_default();
                assert!(said_why.contains(text), "{id}: {said}");
            }
        }
    }

    let bare = start(None);
    let (code, said) = ask(&bare, "linux-2").await;
    assert_eq!(code, 404, "{said}");
}

/// The route is a read: a write to it is refused, and the node's own route is not shadowed
/// (`GET /v1/nodes/{id}` is still the node, 404 here for one that never registered).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_is_a_read_and_the_node_route_is_not_shadowed() {
    let (server, _other, _nodes) = three_members();
    let path = "/v1/nodes/linux-1/storage-health";
    let (code, _) = api_http::post(server.api, path, "{}").await;
    assert_eq!(code, 405);
    let (code, said) = api_http::get(server.api, "/v1/nodes/linux-1").await;
    assert_eq!(code, 404, "{said}");
    assert_eq!(said["error"], "no_node");
}
