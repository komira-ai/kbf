//! The fleet read over HTTP, against fixture children on `127.0.0.1`. A fixture plays the
//! operator port of a child as `docs/api.md` documents it: the two views are open, and the
//! enrollment route passes the gates of a write, in the order the doc gives them (the
//! proxy's secret, no `Origin`, the bearer token, a JSON content type), each refusing with
//! its own status. A test sees what the client sent and the snapshot it made of the answers.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::proxy::child_http::Headers;
use crate::proxy::http_fixture_tests::{Reply, Seen, Server, lock};
use crate::proxy::model::Slot;

use super::fleet::{Enrolled, FleetReader, FleetSnapshot, Population};
use super::fleet_http::{ChildFleet, MAX_ANSWER};

/// What the proxy puts on a call that a child gates on it, and the operator's token.
const SECRET: &str = "abababababababababababababababababababababababababababababababab";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123";

/// Room for a test that is slow only on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(10);

/// The three routes, in the order the reader asks them; an index into [`Answers::routes`].
const PATHS: [&str; 3] = ["/v1/nodes", "/v1/members", "/v1/enroll/nodes"];
const NODES: usize = 0;
const MEMBERS: usize = 1;
const ENROLLED: usize = 2;

// ----- a child's operator port -----

/// What a child answers on each route, and the token its file holds.
#[derive(Clone)]
struct Answers {
    routes: [Reply; 3],
    token: String,
}

impl Answers {
    /// A child with two daemons, two storage members, two nodes enrolled and one revoked.
    fn good() -> Answers {
        Answers {
            routes: [
                Reply::json(200, nodes_view(2, "mac-1", "old-1")),
                Reply::json(200, members_view(2)),
                Reply::json(200, registry()),
            ],
            token: TOKEN.to_owned(),
        }
    }

    /// A child that is not the same one: its daemons speak protocol 3, and it has no members.
    fn other() -> Answers {
        let mut answers = Answers::good();
        answers.routes[NODES] = Reply::json(200, nodes_view(3, "mac-9", "old-9"));
        answers.routes[MEMBERS] = Reply::json(
            200,
            json!({ "generation": 0, "members": [], "oldest_protocol": null,
                    "protocol_unknown": 0 }),
        );
        answers
    }
}

fn node(id: &str, protocol: u32) -> Value {
    json!({ "node_id": id, "connected": true, "protocol": protocol, "software": null,
            "needs_attention": [], "placement": { "state": "serving" } })
}

fn nodes_view(oldest: u32, first: &str, second: &str) -> Value {
    json!({ "server": { "version": "0.1.0+0123456789ab", "commit": "0123456789ab" },
            "nodes": [node(first, oldest), node(second, oldest + 1)],
            "oldest_protocol": oldest, "protocol_unknown": 0 })
}

fn members_view(oldest: u32) -> Value {
    json!({ "generation": 7,
            "members": [
                { "id": 1, "addr": "Store-1.Example:8982", "state": "active", "weight": 1,
                  "protocol": oldest + 1 },
                { "id": 2, "addr": "192.0.2.12:8982", "state": "active", "weight": 1,
                  "protocol": oldest } ],
            "oldest_protocol": oldest, "protocol_unknown": 0 })
}

fn registered(id: &str, roles: &[&str], state: &str, advertise: &str) -> Value {
    json!({ "id": id, "roles": roles, "state": state, "advertise": advertise, "zone": "",
            "token_id": "t", "token_label": "", "created_at": 1, "last_renewed_at": null,
            "not_after": 2, "revoked_at": null })
}

fn registry() -> Value {
    json!({ "nodes": [
        registered("mac-1", &["daemon"], "active", ""),
        registered("store-1", &["storage"], "active", "Store-1.Example:8982"),
        registered("gone-1", &["daemon"], "revoked", ""),
    ] })
}

fn names(of: &[&str]) -> BTreeSet<String> {
    of.iter().map(|s| (*s).to_owned()).collect()
}

/// What [`Answers::good`] must become.
fn good_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        nodes: Ok(Population {
            names: names(&["mac-1", "old-1"]),
            oldest: Some(2),
            unknown: 0,
        }),
        members: Ok(Population {
            names: names(&["store-1.example:8982", "192.0.2.12:8982"]),
            oldest: Some(2),
            unknown: 0,
        }),
        enrolled: Ok(Enrolled {
            daemons: names(&["mac-1"]),
            storage: names(&["store-1.example:8982"]),
        }),
    }
}

/// What [`Answers::other`] becomes.
fn other_snapshot() -> FleetSnapshot {
    FleetSnapshot {
        nodes: Ok(Population {
            names: names(&["mac-9", "old-9"]),
            oldest: Some(3),
            unknown: 0,
        }),
        members: Ok(Population::default()),
        enrolled: good_snapshot().enrolled,
    }
}

/// A fixture child: its operator port, and the answers a test may change while it runs.
struct Child {
    port: Server,
    answers: Arc<Mutex<Answers>>,
}

impl Child {
    fn start() -> Child {
        Child::with(Answers::good())
    }

    fn with(answers: Answers) -> Child {
        Child::hooked(answers, |_| {})
    }

    /// `hook` sees every request before it is answered.
    fn hooked(answers: Answers, mut hook: impl FnMut(&Seen) + Send + 'static) -> Child {
        let answers = Arc::new(Mutex::new(answers));
        let port = {
            let answers = Arc::clone(&answers);
            Server::start(move |request| {
                hook(request);
                route(&lock(&answers), request)
            })
        };
        Child { port, answers }
    }

    fn set(&self, change: impl FnOnce(&mut Answers)) {
        change(&mut lock(&self.answers));
    }

    fn addr(&self) -> SocketAddr {
        self.port.addr
    }

    fn paths(&self) -> Vec<String> {
        self.port.paths()
    }
}

fn route(answers: &Answers, request: &Seen) -> Reply {
    let path = request.path.as_str();
    match (
        request.method.as_str(),
        PATHS.iter().position(|p| *p == path),
    ) {
        ("GET", Some(ENROLLED)) => {
            gate(answers, request).unwrap_or_else(|| answers.routes[ENROLLED].clone())
        }
        ("GET", Some(open)) => answers.routes[open].clone(),
        _ => Reply::json(404, json!({ "error": "no such route" })),
    }
}

/// The gates of a write in the order of `docs/api.md`; `Some` is the refusal.
fn gate(answers: &Answers, request: &Seen) -> Option<Reply> {
    let refuse = |status, why: &str| Some(Reply::json(status, json!({ "error": why })));
    if request.header("x-kbf-proxy-auth") != Some(SECRET) {
        return refuse(403, "a write must come through the proxy");
    }
    if request.header("origin").is_some() {
        return refuse(403, "the request has an Origin header");
    }
    if request.header("authorization") != Some(format!("Bearer {}", answers.token).as_str()) {
        return refuse(401, "unauthorized");
    }
    if request.header("content-type") != Some("application/json") {
        return refuse(415, "the body must be application/json");
    }
    None
}

// ----- readers over fixtures -----

fn secret() -> Headers {
    Headers::new()
        .with("x-kbf-proxy-auth", SECRET.as_bytes())
        .unwrap()
}

fn token() -> io::Result<Vec<u8>> {
    Ok(TOKEN.as_bytes().to_vec())
}

/// A reader of the port at `addr`, whichever slot is active.
fn one(addr: SocketAddr, deadline: Duration) -> ChildFleet {
    ChildFleet::new(|| Some(Slot::A), move |_| addr, secret(), token, deadline)
}

/// A reader whose slot A is the port of `a` and slot B the port of `b`, and whose active
/// slot is whatever `active` holds when it reads.
fn two(a: &Child, b: &Child, active: &Arc<Mutex<Option<Slot>>>) -> ChildFleet {
    let (a, b) = (a.addr(), b.addr());
    let active = Arc::clone(active);
    ChildFleet::new(
        move || *lock(&active),
        move |slot| match slot {
            Slot::A => a,
            Slot::B => b,
        },
        secret(),
        token,
        DEADLINE,
    )
}

/// The text of each read that failed, `None` for each that did not.
fn failed(snapshot: &FleetSnapshot) -> [Option<&str>; 3] {
    [
        snapshot.nodes.as_ref().err(),
        snapshot.members.as_ref().err(),
        snapshot.enrolled.as_ref().err(),
    ]
    .map(|why| why.map(|unreadable| unreadable.0.as_str()))
}

/// A listener that reads each of three requests through its head and answers `bytes`, whatever
/// they are, then closes the connection.
fn answering(bytes: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().take(3).flatten() {
            let mut head = Vec::new();
            let mut buf = [0u8; 512];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(bytes);
        }
    });
    addr
}

/// A port nothing listens on.
fn closed_port() -> SocketAddr {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
}

// ----- the snapshot -----

#[test]
fn the_three_reads_of_the_active_child_become_a_snapshot() {
    let child = Child::start();
    let got = one(child.addr(), DEADLINE).read();
    assert_eq!(got, good_snapshot());
    assert_eq!(
        child.paths(),
        ["GET /v1/nodes", "GET /v1/members", "GET /v1/enroll/nodes"]
    );
}

#[test]
fn the_enrollment_read_carries_every_gate_of_a_write_and_the_views_carry_no_credential() {
    let child = Child::start();
    one(child.addr(), DEADLINE).read();
    let seen = child.port.seen();
    assert_eq!(seen.len(), 3);
    let mut views = Vec::new();
    for view in &seen[..2] {
        for name in [
            "authorization",
            "x-kbf-proxy-auth",
            "origin",
            "content-type",
        ] {
            if view.header(name).is_some() {
                views.push(format!("{} {} sent {name}", view.method, view.path));
            }
        }
    }
    assert!(views.is_empty(), "{views:?}");

    let enrollment = &seen[2];
    assert_eq!(
        enrollment.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(enrollment.header("x-kbf-proxy-auth"), Some(SECRET));
    assert_eq!(enrollment.header("content-type"), Some("application/json"));
    assert_eq!(enrollment.header("origin"), None);
}

// ----- a read that fails -----

#[test]
fn a_read_that_fails_is_unreadable_and_the_other_two_are_still_made() {
    let mut failures = Vec::new();
    for failing in [NODES, MEMBERS, ENROLLED] {
        let child = Child::start();
        child.set(|a| a.routes[failing] = Reply::json(500, json!({ "error": "broken" })));
        let got = one(child.addr(), DEADLINE).read();
        let why = failed(&got);
        let expected = [failing == NODES, failing == MEMBERS, failing == ENROLLED];
        if why.map(|w| w.is_some()) != expected {
            failures.push(format!("{} failing: {why:?}", PATHS[failing]));
        } else if !why[failing].unwrap_or_default().contains(PATHS[failing]) {
            failures.push(format!(
                "{} failing: the text names no route: {why:?}",
                PATHS[failing]
            ));
        }
        if child.paths().len() != 3 {
            failures.push(format!(
                "{} failing: {:?} were asked",
                PATHS[failing],
                child.paths()
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_child_nothing_answers_for_is_unreadable_in_every_field() {
    let got = one(closed_port(), DEADLINE).read();
    let why = failed(&got);
    for (i, path) in PATHS.iter().enumerate() {
        let text = why[i].unwrap_or_default();
        assert!(
            text.contains(path) && text.contains("nothing accepted the connection"),
            "{path}: {text:?}"
        );
    }
}

#[test]
fn a_404_from_the_enrollment_route_is_nobody_enrolled_and_from_a_view_is_unreadable() {
    let child = Child::start();
    child.set(|a| a.routes[ENROLLED] = Reply::json(404, json!({ "error": "no enrollment" })));
    let got = one(child.addr(), DEADLINE).read();
    assert_eq!(got.enrolled, Ok(Enrolled::default()));
    assert_eq!(got.nodes, good_snapshot().nodes);
    assert_eq!(got.members, good_snapshot().members);

    let mut failures = Vec::new();
    for viewed in [NODES, MEMBERS] {
        let child = Child::start();
        child.set(|a| a.routes[viewed] = Reply::json(404, json!({ "error": "no such route" })));
        let got = one(child.addr(), DEADLINE).read();
        let expected = [viewed == NODES, viewed == MEMBERS, false];
        if failed(&got).map(|w| w.is_some()) != expected {
            failures.push(format!("{}: {got:?}", PATHS[viewed]));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn an_enrollment_read_the_child_refuses_is_unreadable_and_the_views_are_still_read() {
    let mut failures = Vec::new();
    for status in [401, 403, 415, 500, 503] {
        let child = Child::start();
        child.set(|a| a.routes[ENROLLED] = Reply::json(status, json!({ "error": "refused" })));
        let got = one(child.addr(), DEADLINE).read();
        let why = failed(&got);
        let named = why[ENROLLED].is_some_and(|t| t.contains(&status.to_string()));
        if why.map(|w| w.is_some()) != [false, false, true] || !named {
            failures.push(format!("{status}: {why:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn an_envelope_without_oldest_protocol_or_protocol_unknown_is_unreadable() {
    fn without(mut body: Value, key: &str) -> Value {
        body.as_object_mut().unwrap().remove(key);
        body
    }
    let cases = [
        (
            NODES,
            "oldest_protocol",
            without(nodes_view(2, "a", "b"), "oldest_protocol"),
        ),
        (
            NODES,
            "protocol_unknown",
            without(nodes_view(2, "a", "b"), "protocol_unknown"),
        ),
        (
            MEMBERS,
            "oldest_protocol",
            without(members_view(2), "oldest_protocol"),
        ),
        (
            MEMBERS,
            "protocol_unknown",
            without(members_view(2), "protocol_unknown"),
        ),
    ];
    let mut failures = Vec::new();
    for (view, key, body) in cases {
        let child = Child::start();
        child.set(|a| a.routes[view] = Reply::json(200, body));
        let got = one(child.addr(), DEADLINE).read();
        let why = failed(&got);
        let expected = [view == NODES, view == MEMBERS, false];
        if why.map(|w| w.is_some()) != expected || !why[view].unwrap_or_default().contains(key) {
            failures.push(format!("{} without {key}: {why:?}", PATHS[view]));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");

    // A key that is there and says `null` is an empty child, which is readable.
    let child = Child::start();
    child.set(|a| {
        a.routes[NODES] = Reply::json(
            200,
            json!({ "nodes": [], "oldest_protocol": null, "protocol_unknown": 0 }),
        );
    });
    assert_eq!(
        one(child.addr(), DEADLINE).read().nodes,
        Ok(Population::default())
    );
}

#[test]
fn an_answer_over_the_cap_is_unreadable_and_one_at_the_cap_is_read() {
    // A valid, empty view whose `pad` brings the body to exactly `len` bytes.
    fn padded(len: usize) -> String {
        let head = r#"{"nodes":[],"oldest_protocol":null,"protocol_unknown":0,"pad":""#;
        let tail = r#""}"#;
        format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
    }
    let child = Child::start();
    child.set(|a| a.routes[NODES] = Reply::text(200, &padded(MAX_ANSWER + 1)));
    let got = one(child.addr(), DEADLINE).read();
    let why = failed(&got);
    assert!(
        why[NODES].is_some_and(|t| t.contains("over the limit")),
        "{why:?}"
    );
    assert_eq!(got.members, good_snapshot().members);

    child.set(|a| a.routes[NODES] = Reply::text(200, &padded(MAX_ANSWER)));
    assert_eq!(
        one(child.addr(), DEADLINE).read().nodes,
        Ok(Population::default())
    );
}

#[test]
fn an_address_that_is_not_loopback_is_never_asked() {
    let off = SocketAddr::from(([192, 0, 2, 1], 80));
    let got = one(off, DEADLINE).read();
    let why = failed(&got);
    assert!(
        why.iter()
            .all(|t| t.is_some_and(|t| t.contains("not a loopback address"))),
        "{why:?}"
    );
}

#[test]
fn an_answer_that_is_not_http_1_1_is_unreadable_in_every_read() {
    let mut failures = Vec::new();
    for bytes in [
        // Not a status line at all.
        &b"SSH-2.0-banner\r\n\r\n"[..],
        // A status line of another version, which would otherwise be a good answer.
        b"HTTP/1.0 200 OK\r\ncontent-length: 2\r\n\r\n{}",
    ] {
        let got = one(answering(bytes), DEADLINE).read();
        let why = failed(&got);
        if !why
            .iter()
            .all(|t| t.is_some_and(|t| t.contains("malformed answer")))
        {
            failures.push(format!("{:?}: {why:?}", String::from_utf8_lossy(bytes)));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// ----- the active child -----

#[test]
fn the_active_child_is_named_at_every_read() {
    let a = Child::start();
    let b = Child::with(Answers::other());
    let active = Arc::new(Mutex::new(Some(Slot::A)));
    let fleet = two(&a, &b, &active);

    assert_eq!(fleet.read(), good_snapshot());
    assert_eq!((a.paths().len(), b.paths().len()), (3, 0));

    *lock(&active) = Some(Slot::B);
    assert_eq!(fleet.read(), other_snapshot());
    assert_eq!((a.paths().len(), b.paths().len()), (3, 3));

    *lock(&active) = Some(Slot::A);
    assert_eq!(fleet.read(), good_snapshot());
    assert_eq!((a.paths().len(), b.paths().len()), (6, 3));
}

#[test]
fn the_three_reads_of_one_snapshot_come_from_one_child() {
    let active = Arc::new(Mutex::new(Some(Slot::A)));
    // Slot A is active when the read begins, and B when A has been asked the first route.
    let a = {
        let active = Arc::clone(&active);
        Child::hooked(Answers::good(), move |request| {
            if request.path == PATHS[NODES] {
                *lock(&active) = Some(Slot::B);
            }
        })
    };
    let b = Child::with(Answers::other());
    let fleet = two(&a, &b, &active);

    assert_eq!(fleet.read(), good_snapshot());
    assert_eq!((a.paths().len(), b.paths().len()), (3, 0));
    // The next read names the child that is active by then.
    assert_eq!(fleet.read(), other_snapshot());
    assert_eq!((a.paths().len(), b.paths().len()), (3, 3));
}

#[test]
fn no_active_child_is_unreadable_in_every_field_and_nothing_is_asked() {
    let (a, b) = (Child::start(), Child::start());
    let active = Arc::new(Mutex::new(None));
    let got = two(&a, &b, &active).read();
    assert_eq!(failed(&got), [Some("no child is active"); 3]);
    assert_eq!((a.paths().len(), b.paths().len()), (0, 0));
}

// ----- the deadline -----

#[test]
fn one_deadline_covers_the_three_reads_so_a_slow_first_read_leaves_the_third_less() {
    let deadline = Duration::from_millis(800);
    let slow = Duration::from_millis(450);
    let child = Child::start();
    child.set(|a| {
        a.routes[NODES].delay = slow;
        a.routes[ENROLLED].delay = slow;
    });
    let began = Instant::now();
    let got = one(child.addr(), deadline).read();
    let took = began.elapsed();

    // Each read alone is inside the deadline; the first and the third together are not.
    assert_eq!(got.nodes, good_snapshot().nodes);
    assert_eq!(got.members, good_snapshot().members);
    let why = failed(&got);
    assert!(
        why[ENROLLED].is_some_and(|t| t.contains("deadline")),
        "{why:?}"
    );
    // What tells a shared deadline from a deadline per read is the third read's refusal
    // above. This bound only catches a read that nothing bounds, so it leaves a loaded
    // machine room.
    assert!(
        took < deadline + Duration::from_secs(2),
        "the read took {took:?} of {deadline:?}"
    );
}

#[test]
fn a_child_that_answers_nothing_costs_one_deadline_and_not_three() {
    let deadline = Duration::from_millis(400);
    let child = Child::start();
    child.set(|a| {
        for route in &mut a.routes {
            route.delay = Duration::from_secs(3);
        }
    });
    let began = Instant::now();
    let got = one(child.addr(), deadline).read();
    let took = began.elapsed();

    let why = failed(&got);
    assert!(
        why.iter()
            .all(|t| t.is_some_and(|t| t.contains("deadline"))),
        "{why:?}"
    );
    // Three deadlines in a row would be `deadline * 3`; this leaves a loaded machine room.
    assert!(
        took < deadline * 5 / 2,
        "three reads took {took:?} of one {deadline:?}"
    );
}

// ----- the token -----

#[test]
fn the_token_is_read_at_every_enrollment_read_and_its_newline_dropped() {
    let child = Child::start();
    let addr = child.addr();
    let file = Arc::new(Mutex::new(format!("{TOKEN}\n")));
    let fleet = {
        let file = Arc::clone(&file);
        let token = move || Ok(lock(&file).clone().into_bytes());
        ChildFleet::new(|| Some(Slot::A), move |_| addr, secret(), token, DEADLINE)
    };
    assert_eq!(fleet.read(), good_snapshot());

    let rotated = "fedcba9876543210fedcba9876543210fedc";
    child.set(|a| a.token = rotated.to_owned());
    *lock(&file) = format!("{rotated}\n");
    assert_eq!(fleet.read(), good_snapshot());
}

/// What goes wrong, if anything, when the token source is `token`: the enrollment read must
/// be unreadable with `expected` in its text and must not be made, and the two views must
/// still be read.
fn token_refused(
    expected: &str,
    token: impl Fn() -> io::Result<Vec<u8>> + Send + 'static,
) -> Vec<String> {
    let child = Child::start();
    let addr = child.addr();
    let fleet = ChildFleet::new(|| Some(Slot::A), move |_| addr, secret(), token, DEADLINE);
    let got = fleet.read();
    let why = failed(&got);
    let mut failures = Vec::new();
    let said = why[ENROLLED].is_some_and(|t| t.contains("operator token") && t.contains(expected));
    if why[NODES].is_some() || why[MEMBERS].is_some() || !said {
        failures.push(format!("{expected}: {why:?}"));
    }
    if child.paths() != ["GET /v1/nodes", "GET /v1/members"] {
        failures.push(format!("{expected}: asked {:?}", child.paths()));
    }
    failures
}

#[test]
fn a_token_that_cannot_be_read_makes_the_enrollment_read_unreadable_and_it_is_not_made() {
    let failures = token_refused("cannot be read", || Err(io::ErrorKind::NotFound.into()));
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_token_that_cannot_be_sent_as_a_header_makes_the_enrollment_read_unreadable_and_it_is_not_made()
{
    // A line break inside a token would end the header and start another.
    let failures = token_refused("cannot be sent", || {
        Ok(format!("{TOKEN}\r\nOrigin: https://example.test").into_bytes())
    });
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn a_token_file_that_holds_nothing_is_refused_by_the_child_and_the_read_is_unreadable() {
    let mut failures = Vec::new();
    for file in ["", "\n", " \t\r\n"] {
        // The child would answer the registry to a request that passed its gates.
        let child = Child::start();
        let addr = child.addr();
        let token = move || Ok(file.as_bytes().to_vec());
        let fleet = ChildFleet::new(|| Some(Slot::A), move |_| addr, secret(), token, DEADLINE);
        let got = fleet.read();
        let why = failed(&got);
        let refused = why[ENROLLED].is_some_and(|t| t.contains("401"));
        if why.map(|w| w.is_some()) != [false, false, true] || !refused {
            failures.push(format!("{file:?}: {why:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
