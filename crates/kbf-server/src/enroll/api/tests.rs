//! The enrollment API, through its handlers: the gate on every route, the secret shown
//! once, revocation seen by the registry, the writer fence. Each test names the defect it
//! catches.

use std::io;
use std::net::SocketAddr;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use kbf_enroll_registry::{
    EnrollRequest, IssuedRole, RegistryError, enroll_txn, list_nodes, list_tokens,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store, StoreError};
use kbf_types::enroll_api::{NodeList, NodeRevoked, TokenCreated, TokenList, TokenRevoked};
use kbf_types::{CaPin, NodeId, RefusalReason, Role, Token, TokenId, TokenSecret};
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};

use super::*;
use crate::enroll::testkit::WritesFenced;
use crate::enroll::{OsRandom, PinError, PinSource, Random};

const BEARER: &str = "operator-token-0123456789abcdef0123456789";

/// Every byte of a fill is the counter, which rises by one per fill: the first token's id
/// bits are all `0`, its secret is thirty-two bytes of `1`, the next token's secret is `3`.
struct Counting(AtomicU8);

impl Random for Counting {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(self.0.fetch_add(1, Ordering::SeqCst));
        Ok(())
    }
}

/// A source whose fills from the `from`th on (counting from 0) fail.
struct FailsFrom {
    calls: AtomicU8,
    from: u8,
}

impl Random for FailsFrom {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.from {
            return Err(io::Error::other("no entropy"));
        }
        buf.fill(7);
        Ok(())
    }
}

fn fails_from(from: u8) -> Arc<dyn Random> {
    Arc::new(FailsFrom {
        calls: AtomicU8::new(0),
        from,
    })
}

struct Pin(Result<CaPin, PinError>);

impl PinSource for Pin {
    fn pin(&self) -> Result<CaPin, PinError> {
        self.0.clone()
    }
}

fn ca_pin() -> CaPin {
    CaPin::from_bytes([9; 32])
}

fn fake() -> Arc<Fake> {
    let clock = Arc::new(ManualClock::default());
    clock.advance(std::time::Duration::from_secs(1_000_000));
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

fn api(store: Arc<dyn Store>, random: Arc<dyn Random>, pin: Result<CaPin, PinError>) -> EnrollApi {
    EnrollApi::new(store, Arc::new(Pin(pin)), random)
}

fn state_with(token: Option<ApiToken>, api: Option<EnrollApi>) -> Arc<EnrollState> {
    Arc::new(EnrollState {
        token,
        enroll: api.map(Arc::new),
    })
}

fn state_over(store: Arc<dyn Store>) -> Arc<EnrollState> {
    state_over_from(store, 0)
}

/// As `state_over`, with the random source's counter starting at `start`: two frontends
/// over one store must not draw the same token id, which the registry refuses as a
/// duplicate before the fence is reached.
fn state_over_from(store: Arc<dyn Store>, start: u8) -> Arc<EnrollState> {
    let api = api(
        store,
        Arc::new(Counting(AtomicU8::new(start))),
        Ok(ca_pin()),
    );
    state_with(Some(ApiToken::for_tests(BEARER)), Some(api))
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 40_000))
}

fn operator() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {BEARER}")).expect("header"),
    );
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h
}

#[derive(Clone, Debug)]
enum Route {
    Create(String),
    List,
    RevokeToken(String),
    Nodes,
    RevokeNode(String),
    Adopt(String, String),
}

impl Route {
    fn all(token: &str, node: &str) -> Vec<Route> {
        vec![
            Route::Create(r#"{"roles":["daemon"]}"#.to_owned()),
            Route::List,
            Route::RevokeToken(token.to_owned()),
            Route::Nodes,
            Route::RevokeNode(node.to_owned()),
            Route::Adopt("a1".to_owned(), r#"{"roles":["daemon"]}"#.to_owned()),
        ]
    }
}

async fn call(state: &Arc<EnrollState>, route: &Route, peer: SocketAddr, h: HeaderMap) -> Response {
    let st = State(Arc::clone(state));
    match route {
        Route::Create(body) => create(st, Caller::direct(peer), h, Bytes::from(body.clone())).await,
        Route::List => list(st, Caller::direct(peer), h).await,
        Route::RevokeToken(id) => {
            revoke_token_route(st, Caller::direct(peer), Path(id.clone()), h).await
        }
        Route::Nodes => nodes(st, Caller::direct(peer), h).await,
        Route::RevokeNode(id) => {
            revoke_node_route(st, Caller::direct(peer), Path(id.clone()), h, Bytes::new()).await
        }
        Route::Adopt(id, body) => {
            adopt_node_route(
                st,
                Caller::direct(peer),
                Path(format!("{id}:adopt")),
                h,
                Bytes::from(body.clone()),
            )
            .await
        }
    }
}

async fn ok(state: &Arc<EnrollState>, route: &Route) -> Response {
    call(state, route, loopback(), operator()).await
}

async fn text(answer: Response) -> (StatusCode, String) {
    let code = answer.status();
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    (code, String::from_utf8(bytes.to_vec()).expect("UTF-8"))
}

async fn json<T: DeserializeOwned>(answer: Response) -> (StatusCode, T) {
    let (code, body) = text(answer).await;
    let value = serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    (code, value)
}

async fn make(state: &Arc<EnrollState>, body: &str) -> TokenCreated {
    let (code, created) = json(ok(state, &Route::Create(body.to_owned())).await).await;
    assert_eq!(code, StatusCode::OK);
    created
}

fn secret_of(created: &TokenCreated) -> Vec<u8> {
    let token = Token::from_str(&created.token).expect("a token");
    token.secret().as_bytes().to_vec()
}

fn node(id: &str) -> NodeId {
    id.parse().expect("a node id")
}

/// Enrolls node `name` with the daemon role the way the frontend's enroll service would,
/// signing with a fixed certificate.
fn enroll(store: &dyn Store, created: &TokenCreated, name: &str) -> Result<(), RefusalReason> {
    enroll_csr(store, created, name, name.as_bytes())
}

/// As [`enroll`], with a CSR of its own (the same token and CSR again is a replay).
fn enroll_csr(
    store: &dyn Store,
    created: &TokenCreated,
    name: &str,
    csr: &[u8],
) -> Result<(), RefusalReason> {
    let id = TokenId::parse(&created.info.id).expect("id");
    let node_id = node(name);
    let seed = name.bytes().fold(0u8, u8::wrapping_add);
    let now = store.now().as_duration().as_secs();
    let mut sign = || {
        Ok(vec![IssuedRole {
            role: Role::Daemon,
            spki_sha256: [seed; 32],
            serial: vec![1, seed],
            not_before: now - 300,
            not_after: now + 7 * 86_400,
            cert_der: vec![seed],
        }])
    };
    enroll_txn(
        store,
        &EnrollRequest {
            token_id: &id,
            secret: &secret_of(created),
            node_id: &node_id,
            csrs: &[(Role::Daemon, csr)],
            advertise: "",
            zone: "z",
        },
        &mut sign,
    )
    .map(drop)
}

// Catches a list or nodes answer that carries the token's secret, its hash or the
// pin-bearing string (a read-only call must never be a second chance at the secret), and a
// create that drops the string or the secret the operator needs: the secret and the string
// are in the create answer, which is not cacheable, and in no other answer; the list holds
// exactly the fields of `TokenInfo`.
#[tokio::test]
async fn the_secret_is_in_the_create_response_only() {
    let store = fake();
    let state = state_over(store.clone());
    let answer = ok(
        &state,
        &Route::Create(r#"{"roles":["storage","daemon"],"max_uses":3,"label":"rack 4"}"#.into()),
    )
    .await;
    assert_eq!(
        answer
            .headers()
            .get("cache-control")
            .map(HeaderValue::as_bytes),
        Some(&b"no-store"[..])
    );
    let (code, created): (_, TokenCreated) = json(answer).await;
    assert_eq!(code, StatusCode::OK);

    let token = Token::from_str(&created.token).expect("the string parses");
    assert_eq!(token.id().as_str(), created.info.id);
    assert_eq!(token.secret().as_bytes(), &[1u8; 32]);
    assert_eq!(token.pin(), &ca_pin());
    assert_eq!(
        created.token.split('.').nth(2),
        Some(created.secret.as_str())
    );
    assert_eq!(created.info.roles, ["storage", "daemon"]);
    assert_eq!((created.info.max_uses, created.info.uses), (3, 0));
    assert_eq!(created.info.label, "rack 4");

    let hash: [u8; 32] = Sha256::digest([1u8; 32]).into();
    let hash_b64 = Token::new(token.id().clone(), TokenSecret::from_bytes(hash), ca_pin())
        .to_line()
        .split('.')
        .nth(2)
        .expect("secret part")
        .to_owned();
    let needles = [
        created.token.clone(),
        created.secret.clone(),
        ca_pin().to_string(),
        hex::encode(hash),
        hash_b64,
    ];

    enroll(&*store, &created, "n1").expect("enrolled");
    for route in [Route::List, Route::Nodes] {
        let (code, body) = text(ok(&state, &route).await).await;
        assert_eq!(code, StatusCode::OK);
        for needle in &needles {
            assert!(!body.contains(needle.as_str()), "{route:?} leaked: {body}");
        }
    }
    let (_, body) = text(ok(&state, &Route::List).await).await;
    let value: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    let keys: Vec<&str> = value["tokens"][0]
        .as_object()
        .expect("a token")
        .keys()
        .map(String::as_str)
        .collect();
    let mut want = vec![
        "id",
        "roles",
        "host_role",
        "max_uses",
        "uses",
        "expires_at",
        "created_at",
        "revoked_at",
        "node_id",
        "node_prefix",
        "names",
        "replace",
        "label",
        "created_by",
    ];
    let mut got = keys;
    want.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, want);
    let (_, list): (_, TokenList) = json(ok(&state, &Route::List).await).await;
    assert_eq!(list.tokens.len(), 1);
    assert_eq!(list.tokens[0].id, created.info.id);
    assert_eq!(list.tokens[0].uses, 1);
    let (_, nodes): (_, NodeList) = json(ok(&state, &Route::Nodes).await).await;
    assert_eq!(nodes.nodes.len(), 1);
    assert_eq!(nodes.nodes[0].token_id, created.info.id);
    // Revocation answers leak nothing either.
    let (_, body) = text(ok(&state, &Route::RevokeToken(created.info.id.clone())).await).await;
    for needle in &needles {
        assert!(!body.contains(needle.as_str()), "{body}");
    }
}

// Catches a route that skips the operator gate (a missing `gate` call): every one of the
// five routes, a GET included, refuses no bearer, a wrong bearer, a peer that is not
// loopback (IPv4 and IPv6), an Origin header, a missing and a wrong Content-Type, and a
// server started without an API token, with the status the gate gives, and the refused
// calls change nothing.
#[tokio::test]
async fn every_route_is_behind_the_operator_gate() {
    let store = fake();
    let state = state_over(store.clone());
    let created = make(&state, r#"{"roles":["daemon"]}"#).await;
    enroll(&*store, &created, "n1").expect("enrolled");
    let routes = Route::all(&created.info.id, "n1");
    let remote_v4 = SocketAddr::from(([192, 0, 2, 7], 5));
    let remote_v6: SocketAddr = "[2001:db8::1]:5".parse().expect("addr");
    let no_writes = state_with(
        None,
        Some(api(
            store.clone(),
            Arc::new(Counting(AtomicU8::new(50))),
            Ok(ca_pin()),
        )),
    );

    for route in &routes {
        let mut cases: Vec<(&str, StatusCode, Response)> = Vec::new();
        let without = |name| {
            let mut h = operator();
            h.remove(name);
            h
        };
        cases.push((
            "no bearer",
            StatusCode::UNAUTHORIZED,
            call(&state, route, loopback(), without(AUTHORIZATION)).await,
        ));
        let mut wrong = operator();
        wrong.insert(AUTHORIZATION, HeaderValue::from_static("Bearer nope"));
        cases.push((
            "wrong bearer",
            StatusCode::UNAUTHORIZED,
            call(&state, route, loopback(), wrong).await,
        ));
        cases.push((
            "remote v4",
            StatusCode::FORBIDDEN,
            call(&state, route, remote_v4, operator()).await,
        ));
        cases.push((
            "remote v6",
            StatusCode::FORBIDDEN,
            call(&state, route, remote_v6, operator()).await,
        ));
        let mut origin = operator();
        origin.insert(ORIGIN, HeaderValue::from_static("http://evil.example"));
        cases.push((
            "origin",
            StatusCode::FORBIDDEN,
            call(&state, route, loopback(), origin).await,
        ));
        cases.push((
            "no content type",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            call(&state, route, loopback(), without(CONTENT_TYPE)).await,
        ));
        let mut plain = operator();
        plain.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        cases.push((
            "text/plain",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            call(&state, route, loopback(), plain).await,
        ));
        cases.push((
            "writes off",
            StatusCode::FORBIDDEN,
            call(&no_writes, route, loopback(), operator()).await,
        ));
        for (what, want, answer) in cases {
            if want == StatusCode::UNAUTHORIZED {
                assert!(answer.headers().contains_key(WWW_AUTHENTICATE), "{what}");
            }
            let (code, body) = text(answer).await;
            assert_eq!(code, want, "{route:?} {what}: {body}");
            assert!(!body.contains(&created.secret), "{what}: {body}");
        }
    }
    // Nothing the refused calls asked for happened.
    let tokens = list_tokens(&*store).expect("tokens");
    assert_eq!(tokens.len(), 1, "a refused create made a token");
    assert!(tokens[0].revoked_at.is_none(), "a refused DELETE revoked");
    let nodes = list_nodes(&*store).expect("nodes");
    assert!(
        nodes.iter().all(|n| n.revoked_at.is_none()),
        "a refused revoke revoked a node"
    );
    assert!(
        nodes.iter().all(|n| n.id.as_str() != "a1"),
        "a refused adopt adopted a node"
    );

    // The same calls with every gate passed are answered: the refusals above are the gate's.
    for route in &routes {
        let (code, body) = text(ok(&state, route).await).await;
        assert_eq!(code, StatusCode::OK, "{route:?}: {body}");
    }
}

// Catches a server without enrollment (no store behind the routes) that answers anyone who
// asks, or tells an unauthenticated caller what it lacks: the gate runs first, and only a
// caller who passes it is told enrollment is off.
#[tokio::test]
async fn without_enrollment_the_gate_still_runs_first() {
    let state = state_with(Some(ApiToken::for_tests(BEARER)), None);
    for route in Route::all("aaaaaaaaaaaa", "n1") {
        let (code, _) = text(call(&state, &route, loopback(), HeaderMap::new()).await).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{route:?}");
        let (code, body) = text(ok(&state, &route).await).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{route:?}");
        assert!(body.contains("enrollment is not enabled"), "{body}");
    }
}

// Catches a revocation that does not reach the registry's enroll path (the token row
// untouched, or a second node still enrolling with it), a revoke that disturbs nodes
// already enrolled, one that is not idempotent, and one that answers an unknown or
// malformed id as success.
#[tokio::test]
async fn delete_revokes_and_a_later_enroll_is_refused() {
    let store = fake();
    let state = state_over(store.clone());
    let created = make(&state, r#"{"roles":["daemon"],"max_uses":2}"#).await;
    enroll(&*store, &created, "n1").expect("first use");

    let revoke = Route::RevokeToken(created.info.id.clone());
    let (code, revoked): (_, TokenRevoked) = json(ok(&state, &revoke).await).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(revoked.id, created.info.id);
    assert_eq!(
        enroll(&*store, &created, "n2"),
        Err(RefusalReason::TokenRevoked)
    );
    let (_, list): (_, TokenList) = json(ok(&state, &Route::List).await).await;
    assert!(list.tokens[0].revoked_at.is_some());
    let (_, nodes): (_, NodeList) = json(ok(&state, &Route::Nodes).await).await;
    assert_eq!(nodes.nodes.len(), 1, "n1 stays enrolled");
    assert_eq!(nodes.nodes[0].state, "active");
    assert_eq!(ok(&state, &revoke).await.status(), StatusCode::OK);

    for id in ["bbbbbbbbbbbb", "not a token id", "AAAAAAAAAAAA"] {
        let (code, body) = text(ok(&state, &Route::RevokeToken(id.into())).await).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{id}: {body}");
    }
}

// Catches a node revoke that answers success without the deny rows, one that can revoke
// twice or an unknown node, and a revoked id that can enroll again.
#[tokio::test]
async fn a_node_revoke_writes_deny_rows_and_burns_the_id() {
    let store = fake();
    let state = state_over(store.clone());
    let created = make(&state, r#"{"roles":["daemon"],"max_uses":5}"#).await;
    enroll(&*store, &created, "n1").expect("enroll");
    let (code, done): (_, NodeRevoked) =
        json(ok(&state, &Route::RevokeNode("n1".into())).await).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(done.id, "n1");
    assert!(done.denied.contains(&"node n1".to_owned()), "{done:?}");
    assert!(done.denied.len() >= 3, "node, serial and key: {done:?}");
    let (_, nodes): (_, NodeList) = json(ok(&state, &Route::Nodes).await).await;
    assert_eq!(nodes.nodes[0].state, "revoked");
    assert!(nodes.nodes[0].revoked_at.is_some());
    assert_eq!(
        enroll_csr(&*store, &created, "n1", b"another key"),
        Err(RefusalReason::NodeRevoked)
    );
    for (id, want) in [
        ("n1", StatusCode::BAD_REQUEST),
        ("n9", StatusCode::NOT_FOUND),
        ("N_1", StatusCode::NOT_FOUND),
    ] {
        let (code, body) = text(ok(&state, &Route::RevokeNode(id.into())).await).await;
        assert_eq!(code, want, "{id}: {body}");
    }
    // A node revoke takes no body, or `{}`.
    let st = State(Arc::clone(&state));
    let with = |body: &'static str| {
        revoke_node_route(
            st.clone(),
            Caller::direct(loopback()),
            Path("n9".into()),
            operator(),
            Bytes::from_static(body.as_bytes()),
        )
    };
    assert_eq!(with("{}").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(with(r#"{"x":1}"#).await.status(), StatusCode::BAD_REQUEST);
}

async fn adopt(state: &Arc<EnrollState>, id: &str, body: &str) -> Response {
    ok(state, &Route::Adopt(id.to_owned(), body.to_owned())).await
}

// Catches an adopt that answers something other than the row the listing shows, one that
// takes a node id or an address twice, one that accepts a request the sibling routes would
// refuse (unknown field, bad JSON, no roles, a storage node with no address, a loopback
// address, a name this frontend serves), and a refused adopt that leaves a row behind.
#[tokio::test]
async fn adopt_answers_the_row_and_refuses_what_enrollment_refuses() {
    let store = fake();
    let state = state_with(
        Some(ApiToken::for_tests(BEARER)),
        Some(
            api(
                store.clone(),
                Arc::new(Counting(AtomicU8::new(0))),
                Ok(ca_pin()),
            )
            .with_frontend_names(&["Front.Example".to_owned()]),
        ),
    );
    let good = r#"{"roles":["storage","daemon"],"advertise":"S1.Example:7000"}"#;
    let (code, row): (_, kbf_types::enroll_api::NodeInfo) =
        json(adopt(&state, "s1", good).await).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        (row.id.as_str(), row.state.as_str(), row.advertise.as_str()),
        ("s1", "active", "s1.example:7000")
    );
    assert_eq!(row.roles, ["storage", "daemon"]);
    assert_eq!((row.token_id.as_str(), row.not_after), ("adopted", 0));
    let (_, listed): (_, NodeList) = json(ok(&state, &Route::Nodes).await).await;
    assert_eq!(listed.nodes, vec![row]);

    for (id, body, want) in [
        ("s1", r#"{"roles":["daemon"]}"#, StatusCode::CONFLICT),
        ("s2", good, StatusCode::CONFLICT),
        (
            "s2",
            r#"{"roles":["storage"],"advertise":"s2.example:7000","x":1}"#,
            StatusCode::BAD_REQUEST,
        ),
        ("s2", "{", StatusCode::BAD_REQUEST),
        ("s2", "", StatusCode::BAD_REQUEST),
        (
            "s2",
            r#"{"advertise":"s2.example:7000"}"#,
            StatusCode::BAD_REQUEST,
        ),
        ("s2", r#"{"roles":[]}"#, StatusCode::BAD_REQUEST),
        ("s2", r#"{"roles":["storage"]}"#, StatusCode::BAD_REQUEST),
        (
            "s2",
            r#"{"roles":["storage"],"advertise":"s2.example"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "s2",
            r#"{"roles":["daemon"],"advertise":"s2.example:7000"}"#,
            StatusCode::BAD_REQUEST,
        ),
        ("s2", r#"{"roles":["updater"]}"#, StatusCode::BAD_REQUEST),
        (
            "s2",
            r#"{"roles":["storage"],"advertise":"127.0.0.1:7000"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "s2",
            r#"{"roles":["storage"],"advertise":"localhost:7000"}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "s2",
            r#"{"roles":["storage"],"advertise":"FRONT.example:7000"}"#,
            StatusCode::BAD_REQUEST,
        ),
        ("N_2", r#"{"roles":["daemon"]}"#, StatusCode::BAD_REQUEST),
        (
            "localhost",
            r#"{"roles":["daemon"]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            "front.example",
            r#"{"roles":["daemon"]}"#,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (code, text) = text(adopt(&state, id, body).await).await;
        assert_eq!(code, want, "{id} {body}: {text}");
    }
    assert_eq!(
        list_nodes(&*store).expect("nodes").len(),
        1,
        "a refused adopt left a row"
    );

    // A stale handle's adopt lands nothing and is told so, as a stale revoke is.
    let stale = state_with(
        Some(ApiToken::for_tests(BEARER)),
        Some(api(
            Arc::new(WritesFenced(store.clone())),
            Arc::new(Counting(AtomicU8::new(9))),
            Ok(ca_pin()),
        )),
    );
    let (code, _) = text(adopt(&stale, "s3", r#"{"roles":["daemon"]}"#).await).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(list_nodes(&*store).expect("nodes").len(), 1);
}

/// Asserts that a store that refuses every write as one of a superseded handle
/// (`StoreError::Fenced`) is answered with 409 "not the signer", not as a 500 or as
/// success: a stale handle's create, DELETE and revoke land nothing; the calls through a
/// handle the store lets write, the control, succeed; reads are not refused. The two
/// frontends draw from different random streams (see `state_over_from`).
#[tokio::test]
async fn a_stale_frontend_cannot_write() {
    let inner = fake();
    let as_store = || -> Arc<dyn Store> { inner.clone() };
    let stale: Arc<dyn Store> = Arc::new(WritesFenced(as_store()));
    let newer = as_store();
    let (old_state, new_state) = (state_over_from(stale, 100), state_over(newer.clone()));

    let created = make(&new_state, r#"{"roles":["daemon"]}"#).await;
    enroll(&*newer, &created, "n1").expect("enroll through the current writer");

    let create_body = r#"{"roles":["daemon"]}"#.to_owned();
    for route in [
        Route::Create(create_body),
        Route::RevokeToken(created.info.id.clone()),
        Route::RevokeNode("n1".into()),
    ] {
        let (code, body) = text(ok(&old_state, &route).await).await;
        assert_eq!(code, StatusCode::CONFLICT, "{route:?}: {body}");
        assert!(body.contains("not the signer"), "{body}");
    }
    let tokens = list_tokens(&*inner).expect("tokens");
    assert_eq!(tokens.len(), 1, "the stale create landed");
    assert!(tokens[0].revoked_at.is_none(), "the stale DELETE landed");
    let nodes = list_nodes(&*inner).expect("nodes");
    assert!(nodes[0].revoked_at.is_none(), "the stale revoke landed");
    for route in [Route::List, Route::Nodes] {
        assert_eq!(ok(&old_state, &route).await.status(), StatusCode::OK);
    }
    // The control: the same calls through the current writer.
    let fresh = make(&new_state, r#"{"roles":["daemon"]}"#).await;
    assert_ne!(fresh.info.id, created.info.id);
    assert_eq!(
        ok(&new_state, &Route::RevokeNode("n1".into()))
            .await
            .status(),
        StatusCode::OK
    );
}

// Catches a create that accepts what the registry or the grammar refuses, or answers a bad
// request with a server error, and one that leaves a token behind when it answers an
// error: each body is refused with 400 and the store holds no token afterwards.
#[tokio::test]
async fn a_bad_create_is_refused_and_stores_nothing() {
    let store = fake();
    let state = state_over(store.clone());
    let day = 86_400;
    let bodies = [
        String::new(),
        "not json".to_owned(),
        "{}".to_owned(),
        r#"{"roles":[]}"#.to_owned(),
        r#"{"roles":["frontend"]}"#.to_owned(),
        r#"{"roles":["storage","storage"]}"#.to_owned(),
        r#"{"roles":["nope"]}"#.to_owned(),
        r#"{"roles":["daemon"],"max_uses":0}"#.to_owned(),
        r#"{"roles":["daemon"],"max_uses":1001}"#.to_owned(),
        r#"{"roles":["daemon"],"expires_in":0}"#.to_owned(),
        format!(r#"{{"roles":["daemon"],"expires_in":{}}}"#, 30 * day + 1),
        r#"{"roles":["daemon"],"node_id":"Bad_ID"}"#.to_owned(),
        r#"{"roles":["daemon"],"max_use":3}"#.to_owned(),
        // An updater's token names its host role, its one node, and has one use; the host
        // role goes with no other role.
        r#"{"roles":["updater"],"node_id":"n7"}"#.to_owned(),
        r#"{"roles":["updater"],"host_role":"daemon"}"#.to_owned(),
        r#"{"roles":["updater"],"host_role":"nope","node_id":"n7"}"#.to_owned(),
        r#"{"roles":["updater"],"host_role":"cache","node_id":"n7"}"#.to_owned(),
        r#"{"roles":["updater"],"host_role":"daemon","node_id":"n7","max_uses":2}"#.to_owned(),
        r#"{"roles":["updater","daemon"],"host_role":"daemon","node_id":"n7"}"#.to_owned(),
        r#"{"roles":["daemon"],"host_role":"daemon"}"#.to_owned(),
    ];
    for body in bodies {
        let (code, answer) = text(ok(&state, &Route::Create(body.clone())).await).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}: {answer}");
        assert!(answer.contains("error"), "{answer}");
    }
    assert!(list_tokens(&*store).expect("tokens").is_empty());

    let good = make(
        &state,
        &format!(
            r#"{{"roles":["daemon"],"expires_in":{day},"node_id":"n7","node_prefix":"n","names":[".example"],"replace":true}}"#
        ),
    )
    .await;
    let info = &good.info;
    assert_eq!(info.expires_at, info.created_at + day);
    assert_eq!(info.node_id.as_deref(), Some("n7"));
    assert_eq!(info.node_prefix.as_deref(), Some("n"));
    assert_eq!(info.names, [".example"]);
    assert!(info.replace);
    assert_eq!(info.created_by, "operator-api");

    let updater = make(
        &state,
        r#"{"roles":["updater"],"host_role":"mac-daemon","node_id":"host-3"}"#,
    )
    .await;
    assert_eq!(updater.info.roles, ["updater"]);
    assert_eq!(updater.info.host_role.as_deref(), Some("mac-daemon"));
    assert_eq!(updater.info.node_id.as_deref(), Some("host-3"));
    assert_eq!(updater.info.max_uses, 1);
    let (_, list): (_, TokenList) = json(ok(&state, &Route::List).await).await;
    let listed = list
        .tokens
        .iter()
        .find(|t| t.id == updater.info.id)
        .expect("listed");
    assert_eq!(listed.host_role.as_deref(), Some("mac-daemon"));
}

// Catches a token stored before the pin or the randomness is known (a row whose string
// can never be shown: the operator could not use it and could not tell it exists): when
// the CA has no pin or the random source fails, the call fails and no row is written.
#[tokio::test]
async fn a_create_that_cannot_build_the_string_stores_nothing() {
    for (random, pin, want) in [
        (
            Arc::new(Counting(AtomicU8::new(0))) as Arc<dyn Random>,
            Err(PinError("the CA is not loaded".into())),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        // The id's bytes fail, then the secret's.
        (
            fails_from(0),
            Ok(ca_pin()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            fails_from(1),
            Ok(ca_pin()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let store = fake();
        let state = state_with(
            Some(ApiToken::for_tests(BEARER)),
            Some(api(store.clone(), random, pin)),
        );
        let route = Route::Create(r#"{"roles":["daemon"]}"#.to_owned());
        let (code, body) = text(ok(&state, &route).await).await;
        assert_eq!(code, want, "{body}");
        assert!(list_tokens(&*store).expect("tokens").is_empty());
    }
}

// Catches a registry failure told with the wrong status (a client retrying what is its
// own mistake, or not retrying a store that is down) or with the store's text.
#[tokio::test]
async fn each_registry_failure_has_its_own_status() {
    let down = RegistryError::Store(StoreError::Failed("disk path /secret/place".into()));
    let table = [
        (RegistryError::Invalid("bad"), StatusCode::BAD_REQUEST),
        (RegistryError::NotFound, StatusCode::NOT_FOUND),
        (
            RegistryError::Corrupt("token"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (RegistryError::Contended, StatusCode::SERVICE_UNAVAILABLE),
        (down, StatusCode::SERVICE_UNAVAILABLE),
        (
            RegistryError::Store(StoreError::Fenced),
            StatusCode::CONFLICT,
        ),
    ];
    for (failure, want) in table {
        let shown = failure.to_string();
        let (code, body) = text(Failure::from(failure).into_response()).await;
        assert_eq!(code, want, "{shown}");
        assert!(!body.contains("/secret/place"), "{body}");
    }
}

// Catches a Debug that prints a secret: the created token, as the API returns it and as
// the CLI holds it, shows its id and nothing else; so does the API object that draws it.
#[tokio::test]
async fn debug_shows_no_secret() {
    let store = fake();
    let state = state_over(store);
    let created = make(&state, r#"{"roles":["daemon"]}"#).await;
    let shown = format!("{created:?} {:?}", state.enroll.as_ref().expect("api"));
    assert!(shown.contains(&created.info.id), "{shown}");
    for needle in [&created.token, &created.secret] {
        assert!(!shown.contains(needle.as_str()), "{shown}");
    }
    assert!(!shown.contains(&ca_pin().to_string()), "{shown}");
}

// Catches a random source that returns the same bytes twice or zeros (every token would be
// guessable): two fills differ and are not zero.
#[test]
fn the_operating_system_source_draws_fresh_bytes() {
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    OsRandom.fill(&mut a).expect("fill");
    OsRandom.fill(&mut b).expect("fill");
    assert_ne!(a, b);
    assert_ne!(a, [0u8; 32]);
}

// Catches routes mounted at other paths than docs/api.md names, or with other methods.
#[test]
fn the_routes_are_the_documented_ones() {
    assert_eq!(TOKENS, "/v1/enroll/tokens");
    assert_eq!(NODES, "/v1/enroll/nodes");
    let _router = routes(None, None);
}

// Catches a panicking store call turning into a hung request or a dropped connection: the
// blocking task's failure is a 500 that says nothing of the panic.
#[tokio::test]
async fn a_panicking_call_is_a_server_error() {
    let failed = blocking(|| -> Result<(), Failure> { panic!("the store call panicked") }).await;
    let (code, body) = text(failed.expect_err("a failure").into_response()).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!body.contains("panicked"), "{body}");
}
