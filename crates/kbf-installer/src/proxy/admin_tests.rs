//! Tests of `ChildAdmin` over HTTP against fixture children on `127.0.0.1`. Each fixture
//! plays a child's two ports (the admin port and the operator API) as the child documents
//! them: the roles and their transitions of `docs/api.md` "The admin calls" (`409` with the
//! view for a call that does not apply, `400` for a bad takeover body, the state store's
//! generation shared by the children), and the operator routes the swap uses, with the
//! gates of a write. A test sees what the client sent (every request is logged) and what it
//! made of the answer; a failure mode is forced onto a path or delayed.

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::CommitId;
use crate::testkit::{A as COMMIT_A, B as COMMIT_B, c};

use super::admin::{Admin, AdminError, Deadlines, OFFER_PAUSE};
use super::child_http::{Headers, HttpError};
use super::http_fixture_tests::{Reply, Seen, Server, lock};
use super::model::{OpenCounts, RoleInfo, RoleState, Slot};
use super::ports::ChildAdmin;

/// What the proxy puts on a call to a child, and the operator's token.
const SECRET: &str = "abababababababababababababababababababababababababababababababab";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123";

// ----- a child as the documentation describes it -----

/// The state store's generation, which every child shares; `None` when it cannot be read.
type Store = Arc<Mutex<Option<u64>>>;

/// One child process's admin state.
struct World {
    role: &'static str,
    held: Option<u64>,
    superseded: bool,
    protocol: u32,
}

/// The operator API of a child, as far as the swap uses it.
struct Operator {
    ready: bool,
    open_executions: u64,
    queued_operations: u64,
    /// The commit the child was built from.
    own: String,
    /// Whether a refusal of a commit that is not the child's own names `"code": "not_own"`.
    coded: bool,
    token: String,
    /// How many offers that pass the gates are still answered `503` ("the update rows are
    /// busy: send it again") before the child takes one.
    busy: u32,
}

fn view(world: &World, store: &Store) -> Value {
    json!({
        "role": world.role,
        "instance_id": "5d1c4a52-8b0e-4f64-9a27-3c0e4f8d9a10",
        "admin_protocol": world.protocol,
        "generation": *lock(store),
        "held_generation": world.held,
        "superseded": world.superseded,
        "open_streams": 0,
        "open_sessions": 0,
        "reapi": "127.0.0.1:8980",
        "worker": null,
        "api": "127.0.0.1:8982",
    })
}

/// `409` with the role view and the reason.
fn refused(world: &World, store: &Store, why: &str) -> Reply {
    let mut body = view(world, store);
    body["error"] = json!(why);
    Reply::json(409, body)
}

/// The admin routes, as `managed/http.rs` of the child serves them.
fn admin_route(world: &mut World, store: &Store, request: &Seen) -> Reply {
    let live = matches!(world.role, "active" | "quiesced");
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/admin/role") => Reply::json(200, view(world, store)),
        ("POST", "/v1/admin/quiesce") if live => {
            world.role = "quiesced";
            Reply::json(200, view(world, store))
        }
        ("POST", "/v1/admin/resume") if live => {
            if world.role == "quiesced" && *lock(store) != world.held {
                return refused(world, store, "the store has moved: another writer holds it");
            }
            world.role = "active";
            Reply::json(200, view(world, store))
        }
        ("POST", "/v1/admin/stand-down") if world.role != "candidate" => {
            world.role = "stood-down";
            Reply::json(200, view(world, store))
        }
        ("POST", "/v1/admin/takeover") => takeover(world, store, &request.body),
        ("POST", _) => refused(world, store, "this call needs another role"),
        _ => Reply::text(404, ""),
    }
}

/// `POST /v1/admin/takeover`: exactly `{"expect": N}` for a candidate, with the store at `N`.
fn takeover(world: &mut World, store: &Store, body: &[u8]) -> Reply {
    let expect = serde_json::from_slice::<BTreeMap<String, u64>>(body)
        .ok()
        .filter(|fields| fields.len() == 1)
        .and_then(|fields| fields.get("expect").copied());
    let Some(expect) = expect else {
        return Reply::json(
            400,
            json!({"error": "the body is {\"expect\": <generation>}"}),
        );
    };
    if world.role != "candidate" {
        return refused(world, store, "this call needs a process that is candidate");
    }
    let mut generation = lock(store);
    if *generation != Some(expect) {
        drop(generation);
        return refused(world, store, "the store is at another generation");
    }
    // The candidate takes the next generation, which the other child no longer holds.
    *generation = Some(expect + 1);
    world.held = *generation;
    world.role = "active";
    drop(generation);
    Reply::json(200, view(world, store))
}

/// The operator routes: `/readyz` and the drain read are open; the desired offer passes the
/// gates of a write (the proxy's secret, no `Origin`, the bearer token, JSON).
fn operator_route(operator: &mut Operator, request: &Seen) -> Reply {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/readyz") if operator.ready => Reply::json(200, json!({"ready": true})),
        ("GET", "/readyz") => Reply::json(503, json!({"ready": false, "failing": []})),
        ("GET", "/v1/frontend/drain") => Reply::json(
            200,
            json!({
                "draining": false,
                "open_executions": operator.open_executions,
                "queued_operations": operator.queued_operations,
            }),
        ),
        ("POST", "/v1/update/desired") => offer(operator, request),
        _ => Reply::text(404, ""),
    }
}

fn offer(operator: &mut Operator, request: &Seen) -> Reply {
    let error = |status, why: &str| Reply::json(status, json!({"error": why}));
    if request.header("x-kbf-proxy-auth") != Some(SECRET) || request.header("origin").is_some() {
        return error(
            403,
            "a write must come through the proxy, without an Origin",
        );
    }
    if request.header("authorization") != Some(format!("Bearer {}", operator.token).as_str()) {
        return error(401, "the bearer token is wrong");
    }
    if request.header("content-type") != Some("application/json") {
        return error(415, "the body must be JSON");
    }
    if operator.busy > 0 {
        operator.busy -= 1;
        return error(503, "the update rows are busy: send it again");
    }
    let commit = serde_json::from_slice::<BTreeMap<String, String>>(&request.body)
        .ok()
        .filter(|fields| fields.len() == 1)
        .and_then(|fields| fields.get("commit").cloned());
    match commit {
        None => error(400, "the body must be {\"commit\": \"<40 hex>\"}"),
        Some(commit) if commit == operator.own => {
            Reply::json(200, json!({"generation": 3, "commit": commit}))
        }
        Some(commit) => {
            let why = format!(
                "{commit} is not this frontend's own release ({}): an offer of one must say it \
                 is a rollback",
                operator.own
            );
            let mut body = json!({"error": why});
            if operator.coded {
                body["code"] = json!("not_own");
            }
            Reply::json(409, body)
        }
    }
}

/// One slot: a child's admin port and operator port, and the hooks a test uses to break them.
struct Child {
    world: Arc<Mutex<World>>,
    operator: Arc<Mutex<Operator>>,
    /// Answers forced onto a path in place of the child's own.
    forced: Arc<Mutex<BTreeMap<String, Reply>>>,
    /// Paths whose answer is held back.
    slowed: Arc<Mutex<BTreeMap<String, Duration>>>,
    admin_port: Server,
    operator_port: Server,
}

impl Child {
    fn new(store: &Store, role: &'static str, held: Option<u64>) -> Child {
        let world = Arc::new(Mutex::new(World {
            role,
            held,
            superseded: false,
            protocol: 1,
        }));
        let operator = Arc::new(Mutex::new(Operator {
            ready: true,
            open_executions: 3,
            queued_operations: 1,
            own: COMMIT_B.to_owned(),
            coded: true,
            token: TOKEN.to_owned(),
            busy: 0,
        }));
        let forced = Arc::new(Mutex::new(BTreeMap::new()));
        let slowed = Arc::new(Mutex::new(BTreeMap::new()));
        let admin_port = {
            let (world, store) = (Arc::clone(&world), Arc::clone(store));
            Server::start(hooked(&forced, &slowed, move |request| {
                admin_route(&mut lock(&world), &store, request)
            }))
        };
        let operator_port = {
            let operator = Arc::clone(&operator);
            Server::start(hooked(&forced, &slowed, move |request| {
                operator_route(&mut lock(&operator), request)
            }))
        };
        Child {
            world,
            operator,
            forced,
            slowed,
            admin_port,
            operator_port,
        }
    }

    fn force(&self, path: &str, reply: Reply) {
        lock(&self.forced).insert(path.to_owned(), reply);
    }

    fn slow(&self, path: &str, delay: Duration) {
        lock(&self.slowed).insert(path.to_owned(), delay);
    }

    fn state(&self) -> (&'static str, Option<u64>) {
        let world = lock(&self.world);
        (world.role, world.held)
    }

    /// Nothing reached this child's two ports.
    fn untouched(&self) -> bool {
        self.admin_port.seen().is_empty() && self.operator_port.seen().is_empty()
    }
}

/// `route`, unless the path is forced, delayed if the path is slowed.
fn hooked(
    forced: &Arc<Mutex<BTreeMap<String, Reply>>>,
    slowed: &Arc<Mutex<BTreeMap<String, Duration>>>,
    mut route: impl FnMut(&Seen) -> Reply + Send + 'static,
) -> impl FnMut(&Seen) -> Reply + Send + 'static {
    let (forced, slowed) = (Arc::clone(forced), Arc::clone(slowed));
    move |request| {
        let forced = lock(&forced).get(&request.path).cloned();
        let mut reply = forced.unwrap_or_else(|| route(request));
        if let Some(delay) = lock(&slowed).get(&request.path) {
            reply.delay = *delay;
        }
        reply
    }
}

/// Two children and a client of them: A is active and holds generation 7, B is a candidate.
struct Rig {
    store: Store,
    a: Child,
    b: Child,
    admin: Admin,
}

fn pick<T: Copy>(slot: Slot, a: T, b: T) -> T {
    match slot {
        Slot::A => a,
        Slot::B => b,
    }
}

impl Rig {
    fn new() -> Rig {
        Rig::with(Deadlines {
            call: Duration::from_secs(10),
            quiesce: Duration::from_secs(10),
        })
    }

    fn with(deadlines: Deadlines) -> Rig {
        let store: Store = Arc::new(Mutex::new(Some(7)));
        let a = Child::new(&store, "active", Some(7));
        let b = Child::new(&store, "candidate", None);
        let admin = client(
            &a,
            &b,
            secret(),
            || Ok(format!("{TOKEN}\n").into_bytes()),
            deadlines,
        );
        Rig { store, a, b, admin }
    }
}

fn secret() -> Headers {
    Headers::new()
        .with("x-kbf-proxy-auth", SECRET.as_bytes())
        .unwrap()
}

/// A client of `a` (slot A) and `b` (slot B).
fn client(
    a: &Child,
    b: &Child,
    secret: Headers,
    token: impl Fn() -> io::Result<Vec<u8>> + Send + Sync + 'static,
    deadlines: Deadlines,
) -> Admin {
    let admin = (a.admin_port.addr, b.admin_port.addr);
    let operator = (a.operator_port.addr, b.operator_port.addr);
    Admin::new(
        move |slot| pick(slot, admin.0, admin.1),
        move |slot| pick(slot, operator.0, operator.1),
        secret,
        token,
        deadlines,
    )
}

fn info(state: RoleState, holder: bool) -> RoleInfo {
    RoleInfo { holder, state }
}

fn took<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let began = Instant::now();
    let out = f();
    (out, began.elapsed())
}

fn short(call: u64, quiesce: u64) -> Deadlines {
    Deadlines {
        call: Duration::from_millis(call),
        quiesce: Duration::from_millis(quiesce),
    }
}

fn offered() -> CommitId {
    c(COMMIT_B)
}

// ----- addressing -----

#[test]
fn a_call_goes_to_the_address_of_the_slot_it_names() {
    // Catches: a slot ignored (every call to one child); the two slots swapped.
    let rig = Rig::new();
    rig.admin.try_role(Slot::A).unwrap();
    assert_eq!(rig.a.admin_port.paths(), ["GET /v1/admin/role"]);
    assert!(rig.b.untouched());
    rig.admin.try_role(Slot::B).unwrap();
    assert_eq!(rig.b.admin_port.paths(), ["GET /v1/admin/role"]);
    assert_eq!(rig.a.admin_port.paths().len(), 1);
}

#[test]
fn the_admin_calls_use_the_admin_port_and_the_reads_and_the_offer_the_operator_port() {
    // Catches: a call on the wrong one of a slot's two ports. A readiness read on the admin
    // port would find no `/readyz` there (404), and the role read no admin route on the
    // operator port.
    let rig = Rig::new();
    let slot = Slot::B;
    rig.admin.try_role(slot).unwrap();
    rig.admin.try_ready(slot).unwrap();
    rig.admin.try_open_counts(slot).unwrap();
    rig.admin.try_post_desired(slot, &offered()).unwrap();
    assert_eq!(rig.b.admin_port.paths(), ["GET /v1/admin/role"]);
    assert_eq!(
        rig.b.operator_port.paths(),
        [
            "GET /readyz",
            "GET /v1/frontend/drain",
            "POST /v1/update/desired"
        ]
    );
    assert!(rig.a.untouched());
}

#[test]
fn an_address_that_is_not_loopback_is_refused_and_nothing_is_sent() {
    // Catches: the secret or the token sent off the host because a table put a port on
    // another address.
    let off = SocketAddr::from(([192, 0, 2, 1], 80));
    let admin = Admin::new(
        move |_| off,
        move |_| off,
        secret(),
        || Ok(TOKEN.as_bytes().to_vec()),
        short(200, 200),
    );
    assert_eq!(
        admin.try_role(Slot::A),
        Err(AdminError::Transport {
            call: "role",
            error: HttpError::NotLoopback(off)
        })
    );
    assert_eq!(
        admin.try_ready(Slot::A),
        Err(AdminError::Transport {
            call: "ready",
            error: HttpError::NotLoopback(off)
        })
    );
    assert_eq!(
        admin.try_post_desired(Slot::A, &offered()),
        Err(AdminError::Transport {
            call: "post_desired",
            error: HttpError::NotLoopback(off)
        })
    );
}

#[test]
fn a_port_nobody_listens_on_is_a_transport_error_and_not_a_timeout() {
    // Catches: a refused connection reported as a timeout (the swap would wait for it).
    let closed = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap()
    };
    let admin = Admin::new(
        move |_| closed,
        move |_| closed,
        secret(),
        || Ok(TOKEN.as_bytes().to_vec()),
        short(2000, 2000),
    );
    let expected = AdminError::Transport {
        call: "role",
        error: HttpError::Refused,
    };
    assert_eq!(admin.try_role(Slot::A), Err(expected));
}

// ----- role and the holder table -----

#[test]
fn holder_follows_the_store_and_not_the_role() {
    // Catches: `holder` read from the role word; `superseded` ignored; the store's
    // generation not compared with the one the child took, or compared as "at least" (a
    // store restored to an older generation would leave the child a holder); a child that
    // cannot read the store taken for a holder (or for a non-holder: the swap would roll
    // back on it).
    // (role, held, store, superseded) -> (state, holder), or None for "cannot say".
    type Row = (
        &'static str,
        Option<u64>,
        Option<u64>,
        bool,
        Option<(RoleState, bool)>,
    );
    let rows: [Row; 10] = [
        (
            "candidate",
            None,
            Some(7),
            false,
            Some((RoleState::Candidate, false)),
        ),
        (
            "candidate",
            None,
            None,
            false,
            Some((RoleState::Candidate, false)),
        ),
        (
            "active",
            Some(7),
            Some(7),
            false,
            Some((RoleState::Active, true)),
        ),
        (
            "active",
            Some(7),
            Some(7),
            true,
            Some((RoleState::Active, false)),
        ),
        (
            "active",
            Some(7),
            Some(8),
            false,
            Some((RoleState::Active, false)),
        ),
        // A store restored to an older generation is not the one the child took.
        (
            "active",
            Some(9),
            Some(7),
            false,
            Some((RoleState::Active, false)),
        ),
        (
            "quiesced",
            Some(7),
            Some(7),
            false,
            Some((RoleState::Quiesced, true)),
        ),
        (
            "stood-down",
            Some(7),
            Some(8),
            false,
            Some((RoleState::StoodDown, false)),
        ),
        (
            "stood-down",
            Some(7),
            Some(7),
            false,
            Some((RoleState::StoodDown, true)),
        ),
        ("active", Some(7), None, false, None),
    ];
    let rig = Rig::new();
    for (role, held, store, superseded, want) in rows {
        {
            let mut world = lock(&rig.a.world);
            world.role = role;
            world.held = held;
            world.superseded = superseded;
        }
        *lock(&rig.store) = store;
        let want = match want {
            Some((state, holder)) => Ok(info(state, holder)),
            None => Err(AdminError::Unreadable { call: "role" }),
        };
        assert_eq!(
            rig.admin.try_role(Slot::A),
            want,
            "{role} held {held:?} store {store:?} superseded {superseded}"
        );
    }
}

#[test]
fn an_answer_the_client_cannot_read_is_a_bad_answer_or_another_protocol() {
    // Catches: a role word the client does not know read as a state; a view without the
    // fields the swap decides on; a child that speaks another protocol read as this one;
    // text where JSON is.
    let rig = Rig::new();
    let good = |role: &str| {
        json!({"role": role, "admin_protocol": 1, "generation": 7, "held_generation": 7,
               "superseded": false})
    };
    let mut other_protocol = good("active");
    other_protocol["admin_protocol"] = json!(2);
    let mut no_flag = good("active");
    no_flag.as_object_mut().unwrap().remove("superseded");
    for (body, shows) in [
        ("not json".to_owned(), "expected"),
        (good("frozen").to_string(), "unknown variant"),
        (no_flag.to_string(), "superseded"),
        (String::new(), "EOF"),
    ] {
        rig.a.force("/v1/admin/role", Reply::text(200, &body));
        let Err(AdminError::BadAnswer { call, why }) = rig.admin.try_role(Slot::A) else {
            panic!("{body:?} is not an answer");
        };
        assert_eq!(call, "role");
        assert!(why.contains(shows), "{why:?} for {body:?}");
    }
    rig.a
        .force("/v1/admin/role", Reply::json(200, other_protocol));
    assert_eq!(
        rig.admin.try_role(Slot::A),
        Err(AdminError::Protocol {
            call: "role",
            found: 2
        })
    );
}

// ----- takeover -----

#[test]
fn takeover_reads_the_generation_and_passes_it_as_expect() {
    // Catches: a takeover that sends no `expect`, a stale or fixed one, or extra fields (the
    // child refuses any other body); the secret or the JSON type left off the POST.
    let rig = Rig::new();
    rig.admin
        .try_takeover(Slot::B, Duration::from_secs(10))
        .unwrap();
    let seen = rig.b.admin_port.seen();
    assert_eq!(
        rig.b.admin_port.paths(),
        ["GET /v1/admin/role", "POST /v1/admin/takeover"]
    );
    assert_eq!(seen[1].body, br#"{"expect":7}"#);
    assert_eq!(seen[1].header("content-type"), Some("application/json"));
    assert_eq!(seen[1].header("x-kbf-proxy-auth"), Some(SECRET));
    assert_eq!(rig.b.state(), ("active", Some(8)));
    // The store moved on, so the old child no longer holds it, though it does not know.
    assert_eq!(
        rig.admin.try_role(Slot::A),
        Ok(info(RoleState::Active, false))
    );
    assert_eq!(
        rig.admin.try_role(Slot::B),
        Ok(info(RoleState::Active, true))
    );
}

#[test]
fn takeover_without_a_generation_sends_nothing() {
    // Catches: a takeover that guesses (0) when the candidate cannot read the store.
    let rig = Rig::new();
    *lock(&rig.store) = None;
    assert_eq!(
        rig.admin.try_takeover(Slot::B, Duration::from_secs(10)),
        Err(AdminError::NoGeneration)
    );
    assert_eq!(rig.b.admin_port.paths(), ["GET /v1/admin/role"]);
}

#[test]
fn the_takeover_limit_is_the_deadline_of_the_call() {
    // Catches: a limit that is dropped (the wire has no field for it, so only the client
    // can enforce it) or replaced by the per-call deadline of 10 s.
    let rig = Rig::new();
    rig.b.slow("/v1/admin/takeover", Duration::from_secs(2));
    let (done, took) = took(|| rig.admin.try_takeover(Slot::B, Duration::from_millis(250)));
    assert_eq!(done, Err(AdminError::Timeout { call: "takeover" }));
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn the_takeover_limit_covers_the_read_of_the_generation_too() {
    // Catches: a limit given to each of the two calls in turn: the read takes 150 ms and the
    // takeover 150 ms, so the two together are over 250 ms, though neither alone is.
    let rig = Rig::new();
    rig.b.slow("/v1/admin/role", Duration::from_millis(150));
    rig.b.slow("/v1/admin/takeover", Duration::from_millis(150));
    let done = rig.admin.try_takeover(Slot::B, Duration::from_millis(250));
    assert_eq!(done, Err(AdminError::Timeout { call: "takeover" }));
}

#[test]
fn a_takeover_of_a_child_that_is_not_a_candidate_is_a_conflict() {
    // Catches: a refusal of the role taken for success; the reason lost.
    let rig = Rig::new();
    let Err(AdminError::Conflict { call, error }) =
        rig.admin.try_takeover(Slot::A, Duration::from_secs(10))
    else {
        panic!("a takeover of the active child must be refused");
    };
    assert_eq!(call, "takeover");
    assert!(error.contains("candidate"), "{error}");
    assert_eq!(rig.a.state(), ("active", Some(7)));
}

// ----- quiesce, stand-down, resume -----

#[test]
fn quiesce_stops_an_active_child_and_is_idempotent() {
    // Catches: a quiesce sent to the wrong route; a role that does not move.
    let rig = Rig::new();
    rig.admin.try_quiesce(Slot::A).unwrap();
    rig.admin.try_quiesce(Slot::A).unwrap();
    assert_eq!(
        rig.admin.try_role(Slot::A),
        Ok(info(RoleState::Quiesced, true))
    );
    assert!(matches!(
        rig.admin.try_quiesce(Slot::B),
        Err(AdminError::Conflict {
            call: "quiesce",
            ..
        })
    ));
}

#[test]
fn quiesce_gives_up_at_the_swaps_budget_and_not_the_childs() {
    // Catches: the quiesce deadline replaced by the per-call one: the child takes up to 10 s,
    // the swap gives it 3 s, and a call that waits past that holds the proxy's streams.
    let rig = Rig::with(Deadlines {
        call: Duration::from_secs(10),
        quiesce: Duration::from_millis(200),
    });
    rig.a.slow("/v1/admin/quiesce", Duration::from_secs(2));
    let (done, took) = took(|| rig.admin.try_quiesce(Slot::A));
    assert_eq!(done, Err(AdminError::Timeout { call: "quiesce" }));
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn resume_puts_a_quiesced_child_back_and_is_idempotent() {
    // Catches: a resume that does not reach the child, or fails on a child that serves.
    let rig = Rig::new();
    rig.admin.try_quiesce(Slot::A).unwrap();
    rig.admin.try_resume(Slot::A).unwrap();
    rig.admin.try_resume(Slot::A).unwrap();
    assert_eq!(
        rig.admin.try_role(Slot::A),
        Ok(info(RoleState::Active, true))
    );
}

#[test]
fn resume_after_another_writer_took_the_generation_says_superseded() {
    // Catches: a refused resume taken for a plain conflict, or for success: the old child is
    // quiesced, the candidate took the generation, and the old child cannot take it back.
    let rig = Rig::new();
    rig.admin.try_quiesce(Slot::A).unwrap();
    rig.admin
        .try_takeover(Slot::B, Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        rig.admin.try_resume(Slot::A),
        Err(AdminError::Superseded { call: "resume" })
    );
    assert_eq!(rig.a.state().0, "quiesced");
}

#[test]
fn resume_after_a_stand_down_says_stood_down_to_the_swap() {
    // Catches: a resume that reports success for a child that cannot serve (a rollback that
    // then releases the hold into a stood-down child); the reason lost on the way to the swap.
    let rig = Rig::new();
    rig.admin.try_stand_down(Slot::A).unwrap();
    assert_eq!(
        rig.admin.try_role(Slot::A),
        Ok(info(RoleState::StoodDown, true))
    );
    assert_eq!(
        rig.admin.try_resume(Slot::A),
        Err(AdminError::StoodDown { call: "resume" })
    );
    let port = ChildAdmin::resume(&rig.admin, Slot::A).unwrap_err();
    assert!(
        port.0
            .starts_with("resume: the child stood down, which is never undone"),
        "{}",
        port.0
    );
    assert_eq!(rig.a.state().0, "stood-down");
}

#[test]
fn a_rollback_after_the_swap_cannot_resume_the_old_child() {
    // The swap as the child sees it, rollback included: the new child takes over, the old
    // one is told to stand down (step 7), and a rollback has nothing to resume.
    let rig = Rig::new();
    rig.admin.try_quiesce(Slot::A).unwrap();
    rig.admin
        .try_takeover(Slot::B, Duration::from_secs(10))
        .unwrap();
    rig.admin.try_stand_down(Slot::A).unwrap();
    assert_eq!(
        rig.admin.try_role(Slot::B),
        Ok(info(RoleState::Active, true))
    );
    // The rollback: the new child stands down, the old one is asked to resume.
    rig.admin.try_stand_down(Slot::B).unwrap();
    assert_eq!(
        rig.admin.try_resume(Slot::A),
        Err(AdminError::StoodDown { call: "resume" })
    );
}

#[test]
fn a_candidate_cannot_stand_down_and_a_refusal_without_a_view_is_a_conflict() {
    // Catches: a refusal that names no role taken for a stood-down or superseded child.
    let rig = Rig::new();
    assert!(matches!(
        rig.admin.try_stand_down(Slot::B),
        Err(AdminError::Conflict {
            call: "stand_down",
            ..
        })
    ));
    rig.a
        .force("/v1/admin/stand-down", Reply::text(409, "busy"));
    assert_eq!(
        rig.admin.try_stand_down(Slot::A),
        Err(AdminError::Conflict {
            call: "stand_down",
            error: String::new()
        })
    );
}

// ----- statuses -----

#[test]
fn each_status_of_an_admin_call_has_its_own_error() {
    // Catches: two statuses mapped to each other's errors (a 404 is a child with no admin
    // listener, a 500 a transition that failed); a reason lost, or not cut; a non-JSON body
    // that breaks the mapping instead of leaving the reason empty.
    let rig = Rig::new();
    let long = "x".repeat(500);
    let call = |rig: &Rig| rig.admin.try_stand_down(Slot::A);
    let table: [(Reply, AdminError); 6] = [
        (
            Reply::text(404, ""),
            AdminError::NotFound { call: "stand_down" },
        ),
        (
            Reply::json(500, json!({"error": "could not start"})),
            AdminError::Failed {
                call: "stand_down",
                error: "could not start".into(),
            },
        ),
        (
            Reply::json(500, json!({"error": long})),
            AdminError::Failed {
                call: "stand_down",
                error: "x".repeat(200),
            },
        ),
        (
            Reply::text(500, "<html>"),
            AdminError::Failed {
                call: "stand_down",
                error: String::new(),
            },
        ),
        (
            Reply::json(503, json!({"error": "later"})),
            AdminError::Status {
                call: "stand_down",
                code: 503,
                error: "later".into(),
            },
        ),
        (
            Reply::text(400, ""),
            AdminError::Status {
                call: "stand_down",
                code: 400,
                error: String::new(),
            },
        ),
    ];
    for (reply, want) in table {
        let status = reply.status;
        rig.a.force("/v1/admin/stand-down", reply);
        assert_eq!(call(&rig), Err(want), "status {status}");
    }
}

#[test]
fn a_2xx_answer_that_is_not_a_role_view_fails_the_call() {
    // Catches: a call taken for done because the status was 2xx, when the body shows the
    // child is not what the client speaks to.
    let rig = Rig::new();
    rig.a.force("/v1/admin/quiesce", Reply::text(200, "ok"));
    assert!(matches!(
        rig.admin.try_quiesce(Slot::A),
        Err(AdminError::BadAnswer {
            call: "quiesce",
            ..
        })
    ));
    rig.b.force("/v1/admin/takeover", Reply::text(200, "{}"));
    assert!(matches!(
        rig.admin.try_takeover(Slot::B, Duration::from_secs(10)),
        Err(AdminError::BadAnswer {
            call: "takeover",
            ..
        })
    ));
}

#[test]
fn a_child_that_does_not_answer_is_a_timeout() {
    // Catches: a call that waits for ever, or a timeout reported as another error.
    let rig = Rig::with(short(200, 200));
    rig.a.slow("/v1/admin/role", Duration::from_secs(2));
    let (done, took) = took(|| rig.admin.try_role(Slot::A));
    assert_eq!(done, Err(AdminError::Timeout { call: "role" }));
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn every_call_that_does_not_wait_for_work_gives_up_at_the_per_call_deadline() {
    // Catches: a call whose deadline is not `Deadlines::call` (a constant, the quiesce
    // budget, or none): the swap would wait on a hung child as long as that child likes.
    // The quiesce budget is 5 s here, so a call that took it for its own would answer
    // after the 2 s the child holds it back, and not time out.
    type Call = fn(&Admin) -> Result<(), AdminError>;
    let calls: [(&str, &str, Call); 5] = [
        ("stand_down", "/v1/admin/stand-down", |admin| {
            admin.try_stand_down(Slot::A)
        }),
        ("resume", "/v1/admin/resume", |admin| {
            admin.try_resume(Slot::A)
        }),
        ("ready", "/readyz", |admin| {
            admin.try_ready(Slot::A).map(drop)
        }),
        ("open_counts", "/v1/frontend/drain", |admin| {
            admin.try_open_counts(Slot::A).map(drop)
        }),
        ("post_desired", "/v1/update/desired", |admin| {
            admin.try_post_desired(Slot::A, &offered())
        }),
    ];
    // Every call is tried, so a defect in several names all of them.
    let mut late = Vec::new();
    for (call, path, run) in calls {
        let rig = Rig::with(short(200, 5000));
        rig.a.slow(path, Duration::from_secs(2));
        let (done, took) = took(|| run(&rig.admin));
        if done != Err(AdminError::Timeout { call }) || took >= Duration::from_millis(1500) {
            late.push(format!("{call}: {done:?} after {took:?}"));
        }
    }
    assert!(late.is_empty(), "{late:#?}");
}

// ----- ready and open_counts -----

#[test]
fn ready_asks_readyz_of_the_operator_port_with_no_credential_and_is_not_whole_yet() {
    // Catches: `/readyz` asked of the admin port, or with the secret or the token; 503 read
    // as ready; any other status read as an answer; and `health`/`capabilities` claimed
    // without a probe of them (`all()` must stay false until those exist).
    let rig = Rig::new();
    let ready = rig.admin.try_ready(Slot::B).unwrap();
    assert!(ready.readyz);
    assert!(!ready.health && !ready.capabilities && !ready.all());
    let seen = rig.b.operator_port.seen();
    assert_eq!(rig.b.operator_port.paths(), ["GET /readyz"]);
    assert!(seen[0].header("x-kbf-proxy-auth").is_none());
    assert!(seen[0].header("authorization").is_none());
    assert!(rig.b.admin_port.seen().is_empty());

    lock(&rig.b.operator).ready = false;
    assert!(!rig.admin.try_ready(Slot::B).unwrap().readyz);

    // Whatever else answers is not the operator API.
    rig.b
        .force("/readyz", Reply::json(500, json!({"error": "boom"})));
    assert_eq!(
        rig.admin.try_ready(Slot::B),
        Err(AdminError::Status {
            call: "ready",
            code: 500,
            error: "boom".into()
        })
    );
    rig.b.force("/readyz", Reply::text(204, ""));
    assert!(matches!(
        rig.admin.try_ready(Slot::B),
        Err(AdminError::Status { code: 204, .. })
    ));
}

#[test]
fn readiness_is_not_read_from_the_admin_port() {
    // Catches: a client pointed at the wrong port: the admin port has no `/readyz`.
    let rig = Rig::new();
    let admin_port = rig.b.admin_port.addr;
    let admin = Admin::new(
        move |_| admin_port,
        move |_| admin_port,
        secret(),
        || Ok(TOKEN.as_bytes().to_vec()),
        short(2000, 2000),
    );
    assert!(matches!(
        admin.try_ready(Slot::B),
        Err(AdminError::Status {
            call: "ready",
            code: 404,
            ..
        })
    ));
}

#[test]
fn open_counts_reads_the_drain_endpoint_with_no_credential() {
    // Catches: the two counts swapped or one dropped; a read that carries the secret it
    // does not need.
    let rig = Rig::new();
    assert_eq!(
        rig.admin.try_open_counts(Slot::A),
        Ok(OpenCounts {
            open_executions: 3,
            queued_operations: 1
        })
    );
    let seen = rig.a.operator_port.seen();
    assert_eq!(rig.a.operator_port.paths(), ["GET /v1/frontend/drain"]);
    assert!(seen[0].header("x-kbf-proxy-auth").is_none());
    rig.a.force(
        "/v1/frontend/drain",
        Reply::json(200, json!({"draining": true, "open_executions": 0})),
    );
    assert!(matches!(
        rig.admin.try_open_counts(Slot::A),
        Err(AdminError::BadAnswer {
            call: "open_counts",
            ..
        })
    ));
}

// ----- post_desired -----

#[test]
fn post_desired_carries_the_gates_of_a_write_and_names_only_the_commit() {
    // Catches: the secret, the bearer token or the JSON type missing (the child refuses a
    // write without each); the token's newline sent; an Origin header; a body with more
    // than the commit; the offer sent to the other slot's child.
    let rig = Rig::new();
    rig.admin.try_post_desired(Slot::B, &offered()).unwrap();
    let seen = rig.b.operator_port.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/update/desired");
    assert_eq!(seen[0].header("x-kbf-proxy-auth"), Some(SECRET));
    assert_eq!(
        seen[0].header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(seen[0].header("content-type"), Some("application/json"));
    assert!(seen[0].header("origin").is_none());
    assert_eq!(
        seen[0].body,
        format!(r#"{{"commit":"{COMMIT_B}"}}"#).into_bytes()
    );
    assert!(rig.a.untouched());
}

#[test]
fn post_desired_goes_to_the_child_of_the_slot_and_never_to_the_other() {
    // Catches: the offer posted to the old child (the slot's other) because a swap
    // confuses old and new: the old child is stood down and would answer for a release it
    // does not run.
    let rig = Rig::new();
    rig.admin.try_post_desired(Slot::A, &offered()).unwrap();
    assert_eq!(rig.a.operator_port.paths(), ["POST /v1/update/desired"]);
    assert!(rig.b.untouched());
}

#[test]
fn post_desired_reads_the_token_on_every_call() {
    // Catches: a token read once at construction: a rotated token (the file is replaced)
    // would be refused until the proxy restarts.
    let rig = Rig::new();
    let token = Arc::new(Mutex::new(TOKEN.to_owned()));
    let source = Arc::clone(&token);
    let admin = client(
        &rig.a,
        &rig.b,
        secret(),
        move || Ok(lock(&source).clone().into_bytes()),
        short(2000, 2000),
    );
    admin.try_post_desired(Slot::B, &offered()).unwrap();
    let rotated = "fedcba9876543210fedcba9876543210fedcba98";
    *lock(&token) = rotated.to_owned();
    assert!(matches!(
        admin.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Status { code: 401, .. })
    ));
    lock(&rig.b.operator).token = rotated.to_owned();
    admin.try_post_desired(Slot::B, &offered()).unwrap();
}

#[test]
fn a_409_that_names_not_own_is_told_from_the_other_409s() {
    // Catches: the refusal of a commit that is not the child's own taken for the busy kind
    // (a conflict) or the other way round; the distinction keyed on the text, not the code.
    let rig = Rig::new();
    let foreign = c(COMMIT_A);
    let Err(AdminError::NotOwn { error }) = rig.admin.try_post_desired(Slot::B, &foreign) else {
        panic!("a commit the child was not built from is not its own");
    };
    assert!(error.contains("not this frontend's own release"), "{error}");

    // The refusal as it is today: the same text, and no `code`.
    lock(&rig.b.operator).coded = false;
    assert!(matches!(
        rig.admin.try_post_desired(Slot::B, &foreign),
        Err(AdminError::Conflict {
            call: "post_desired",
            ..
        })
    ));
    // Another 409 with a code of its own, and one with no body.
    rig.b.force(
        "/v1/update/desired",
        Reply::json(
            409,
            json!({"error": "no platform complete", "code": "incomplete"}),
        ),
    );
    assert!(matches!(
        rig.admin.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Conflict {
            call: "post_desired",
            ..
        })
    ));
    rig.b.force("/v1/update/desired", Reply::text(409, ""));
    assert!(matches!(
        rig.admin.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Conflict {
            call: "post_desired",
            ..
        })
    ));
}

#[test]
fn post_desired_refusals_keep_their_status() {
    // Catches: a gate's refusal (token, secret, a write that raced) reported as success or
    // as another kind of error.
    let rig = Rig::new();
    let wrong_secret = Headers::new()
        .with("x-kbf-proxy-auth", b"not the secret")
        .unwrap();
    let guest = client(
        &rig.a,
        &rig.b,
        wrong_secret,
        || Ok(TOKEN.as_bytes().to_vec()),
        short(2000, 2000),
    );
    assert!(matches!(
        guest.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Status { code: 403, .. })
    ));
    let no_token = client(
        &rig.a,
        &rig.b,
        secret(),
        || Ok(b"wrong".to_vec()),
        short(2000, 2000),
    );
    assert!(matches!(
        no_token.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Status { code: 401, .. })
    ));
    // A status that is not one of the gates' and not a busy child is not sent again.
    rig.b.force(
        "/v1/update/desired",
        Reply::json(502, json!({"error": "a proxy in the way"})),
    );
    assert_eq!(
        rig.admin.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Status {
            call: "post_desired",
            code: 502,
            error: "a proxy in the way".into()
        })
    );
    assert_eq!(
        rig.b.operator_port.seen().len(),
        3,
        "403, 401 and 502: one try each"
    );
}

#[test]
fn a_busy_child_is_asked_again_after_a_pause() {
    // Catches: a `503` ("the update rows are busy: send it again") taken for the end of
    // the offer, which leaves a node update silently skipped by one lost race; a retry with
    // no pause (the child asked for one second); a retry sent to the other slot's child.
    let rig = Rig::new();
    lock(&rig.b.operator).busy = 2;
    let (done, took) = took(|| rig.admin.try_post_desired(Slot::B, &offered()));
    assert_eq!(done, Ok(()));
    assert!(took >= 2 * OFFER_PAUSE, "{took:?}");
    assert_eq!(
        rig.b.operator_port.paths(),
        ["POST /v1/update/desired"; 3],
        "two busy answers and the one the child took"
    );
    assert!(rig.a.untouched());
    let seen = rig.b.operator_port.seen();
    assert!(
        seen.iter().all(|s| s.body == seen[0].body),
        "the same offer each time"
    );
}

#[test]
fn a_child_that_stays_busy_is_asked_three_times_and_no_more() {
    // Catches: a retry without a bound (a child without a state store answers 503 for ever);
    // the bound off by one either way; the last answer lost for a made-up one.
    let rig = Rig::new();
    rig.b.force(
        "/v1/update/desired",
        Reply::json(
            503,
            json!({"error": "the update rows are busy: send it again"}),
        ),
    );
    let done = rig.admin.try_post_desired(Slot::B, &offered());
    assert_eq!(
        done,
        Err(AdminError::Status {
            call: "post_desired",
            code: 503,
            error: "the update rows are busy: send it again".into()
        })
    );
    assert_eq!(rig.b.operator_port.seen().len(), 3);
}

#[test]
fn a_busy_child_is_not_asked_again_when_the_pause_does_not_fit_in_the_deadline() {
    // Catches: a retry that ignores the call's deadline: with 1.8 s, the first pause fits
    // (the second try is at 1 s) and the second does not (a third try would start at 2 s),
    // so the offer ends with the second answer, and not with a third try or a timeout.
    let rig = Rig::with(short(1800, 1800));
    rig.b.force(
        "/v1/update/desired",
        Reply::json(
            503,
            json!({"error": "the update rows are busy: send it again"}),
        ),
    );
    let (done, took) = took(|| rig.admin.try_post_desired(Slot::B, &offered()));
    assert!(
        matches!(done, Err(AdminError::Status { code: 503, .. })),
        "{done:?}"
    );
    assert_eq!(rig.b.operator_port.seen().len(), 2);
    assert!(took < Duration::from_millis(1800), "{took:?}");
}

#[test]
fn a_busy_child_is_not_asked_again_when_the_first_pause_does_not_fit() {
    // Catches: a pause taken when the deadline is shorter than it: the offer would end in
    // a timeout, which says "may have landed", for an answer that said it did not.
    let rig = Rig::with(short(500, 500));
    rig.b.force(
        "/v1/update/desired",
        Reply::json(
            503,
            json!({"error": "the update rows are busy: send it again"}),
        ),
    );
    let (done, took) = took(|| rig.admin.try_post_desired(Slot::B, &offered()));
    assert!(
        matches!(done, Err(AdminError::Status { code: 503, .. })),
        "{done:?}"
    );
    assert_eq!(rig.b.operator_port.seen().len(), 1);
    assert!(took < OFFER_PAUSE, "{took:?}");
}

#[test]
fn a_token_that_cannot_be_used_stops_the_offer_before_it_is_sent() {
    // Catches: an offer sent without a token when the file is gone; a token with a line
    // break in it sent as a second header.
    let rig = Rig::new();
    let gone = client(
        &rig.a,
        &rig.b,
        secret(),
        || Err(io::ErrorKind::NotFound.into()),
        short(2000, 2000),
    );
    assert_eq!(
        gone.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Token(io::ErrorKind::NotFound))
    );
    let injected = client(
        &rig.a,
        &rig.b,
        secret(),
        || Ok(b"tok\r\nX-Kbf-Role: operator".to_vec()),
        short(2000, 2000),
    );
    assert!(matches!(
        injected.try_post_desired(Slot::B, &offered()),
        Err(AdminError::Transport {
            call: "post_desired",
            error: HttpError::BadRequest(_)
        })
    ));
    assert!(rig.b.untouched());
}

// ----- through the trait -----

#[test]
fn through_the_trait_every_call_returns_its_value_and_errors_become_text() {
    // Catches: a trait method wired to another call; an error that loses its call name or
    // its reason on the way to the swap, which keeps only text.
    let rig = Rig::new();
    let admin: &dyn ChildAdmin = &rig.admin;
    assert_eq!(admin.role(Slot::A), Ok(info(RoleState::Active, true)));
    assert!(!admin.ready(Slot::B).unwrap().all());
    assert_eq!(
        admin.open_counts(Slot::A),
        Ok(OpenCounts {
            open_executions: 3,
            queued_operations: 1
        })
    );
    admin.post_desired(Slot::B, &offered()).unwrap();
    admin.quiesce(Slot::A).unwrap();
    admin.takeover(Slot::B, Duration::from_secs(10)).unwrap();
    admin.stand_down(Slot::A).unwrap();
    let refused = admin.resume(Slot::A).unwrap_err();
    assert_eq!(
        refused.0,
        "resume: the child stood down, which is never undone; it cannot serve again"
    );
    // B took the generation, so it is not a candidate any more.
    let again = admin
        .takeover(Slot::B, Duration::from_secs(10))
        .unwrap_err();
    assert!(
        again.0.starts_with("takeover: the call does not apply: "),
        "{}",
        again.0
    );
}

#[test]
fn deadlines_for_a_frontend_are_the_swaps_quiesce_budget_and_three_seconds() {
    // Catches: a quiesce deadline that drifts from the swap's own budget.
    let deadlines = Deadlines::frontend();
    assert_eq!(deadlines.quiesce, super::model::Timing::frontend().quiesce);
    assert_eq!(deadlines.call, Duration::from_secs(3));
}
