use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame};
use hyper::{Request, StatusCode};
use kbf_updater::wire::{HEARTBEAT_PATH, MAX_HEARTBEAT_BYTES};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, SanType};
use serde_json::{Value, json};

use crate::http::{
    CONVERGENCE_PATH, DESIRED_PATH, MAX_DESIRED_BYTES, Peer, STATUS_PATH, Surface, handle,
};
use crate::testutil::{OPERATOR, Rig, ci_token};

/// How long a body has in the tests that are not about patience.
mod extras;
mod features;
mod services;
mod status;

const PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

const STATE: &str = r#"{"version":"2.0","min_up":{"storage":1,"frontend":1}}"#;

fn san(sans: &[&str]) -> Vec<SanType> {
    sans.iter()
        .map(|s| match s.strip_prefix("dns:") {
            Some(name) => SanType::DnsName(name.try_into().expect("a name")),
            None => SanType::URI((*s).try_into().expect("a uri")),
        })
        .collect()
}

fn peer(sans: &[&str]) -> Peer {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = san(sans);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let key = KeyPair::generate().expect("key");
    Some(Arc::from(
        params.self_signed(&key).expect("sign").der().as_ref(),
    ))
}

fn updater(name: &str, host_role: &str) -> Peer {
    peer(&[
        "urn:kbf:role:updater",
        &format!("urn:kbf:host-role:{host_role}"),
        &format!("dns:{name}"),
    ])
}

const BEAT: &str = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy"}"#;

fn request(method: &str, path: &str, bearer: Option<&str>, body: &str) -> Request<Full<Bytes>> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("a request")
}

async fn call(
    rig: &Rig,
    surface: Surface,
    peer: &Peer,
    request: Request<Full<Bytes>>,
) -> (StatusCode, Value) {
    answer(handle(&rig.coordinator, surface, peer, request, PATIENCE).await).await
}

async fn answer(response: hyper::Response<Full<Bytes>>) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (status, serde_json::from_slice(&bytes).expect("JSON"))
}

fn hosts(status: &Value) -> Vec<(String, String)> {
    status["hosts"]
        .as_array()
        .expect("hosts")
        .iter()
        .map(|h| {
            (
                h["node_id"].as_str().expect("id").to_owned(),
                h["role"].as_str().expect("role").to_owned(),
            )
        })
        .collect()
}

async fn status(rig: &Rig) -> Value {
    let (code, body) = call(
        rig,
        Surface::Control,
        &None,
        request("GET", STATUS_PATH, Some(OPERATOR), ""),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    body
}

/// Catches: who is calling taken from the body instead of the certificate: a heartbeat whose
/// body claims another node id, another role and another host role is noted under what the
/// certificate names, and the host the body names is not known.
#[tokio::test]
async fn identity_is_taken_from_the_certificate_when_the_body_disagrees() {
    let rig = Rig::new("http-identity");
    let lying = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy",
        "node_id":"frontend-1","role":"frontend","host_role":"frontend","host-role":"frontend"}"#;
    let who = updater("real-host", "storage");
    let (code, _) = call(
        &rig,
        Surface::Heartbeat,
        &who,
        request("POST", HEARTBEAT_PATH, None, lying),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        hosts(&status(&rig).await),
        [("real-host".to_owned(), "storage".to_owned())]
    );
}

/// Catches: a certificate that is not an updater's being served: a storage node's, a
/// frontend's, the coordinator's, an unknown role, a host with no host role or two, no node
/// id, bytes that are no certificate, and no certificate at all. Each is told 403 and the
/// host is not noted.
#[tokio::test]
async fn a_forged_or_unknown_certificate_role_is_refused() {
    let rig = Rig::new("http-forged");
    let cases: Vec<Peer> = vec![
        peer(&[
            "urn:kbf:role:storage",
            "urn:kbf:host-role:storage",
            "dns:h1",
        ]),
        peer(&[
            "urn:kbf:role:frontend",
            "urn:kbf:host-role:frontend",
            "dns:h1",
        ]),
        peer(&["urn:kbf:role:coordinator", "dns:h1"]),
        peer(&["urn:kbf:role:root", "urn:kbf:host-role:storage", "dns:h1"]),
        peer(&["urn:kbf:role:updater", "dns:h1"]),
        peer(&[
            "urn:kbf:role:updater",
            "urn:kbf:host-role:storage",
            "urn:kbf:host-role:daemon",
            "dns:h1",
        ]),
        peer(&["urn:kbf:role:updater", "urn:kbf:host-role:storage"]),
        Some(Arc::from(&b"not DER"[..])),
        None,
    ];
    for who in &cases {
        let beat = request("POST", HEARTBEAT_PATH, None, BEAT);
        let (code, body) = call(&rig, Surface::Heartbeat, who, beat).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"error": "forbidden"}));
    }
    assert!(hosts(&status(&rig).await).is_empty());
}

/// Catches: a reply that carries anything but a version: over HTTP, for a host to be told
/// nothing and for one to be told to update, the body has only the keys `reply` and
/// `version`, and the version is the desired one.
#[tokio::test]
async fn the_reply_never_carries_anything_but_a_version() {
    let rig = Rig::new("http-reply");
    let who = updater("d1", "daemon");
    let (_, nothing) = call(
        &rig,
        Surface::Heartbeat,
        &who,
        request("POST", HEARTBEAT_PATH, None, BEAT),
    )
    .await;
    assert_eq!(nothing, json!({"reply": "nothing"}));
    rig.set(STATE).await;
    rig.at(2_000);
    let (code, update) = call(
        &rig,
        Surface::Heartbeat,
        &who,
        request("POST", HEARTBEAT_PATH, None, BEAT),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(update, json!({"reply": "update", "version": "2.0"}));
    let keys: Vec<&String> = update.as_object().expect("object").keys().collect();
    assert_eq!(keys, ["reply", "version"]);
}

/// Catches: a malformed heartbeat taking the coordinator down or being noted: not JSON, JSON
/// of another shape, a version the host-side rule refuses, a body over the cap, and a body
/// that fails while it is read are all 400, and no host is noted.
#[tokio::test]
async fn a_malformed_heartbeat_is_a_bad_request() {
    let rig = Rig::new("http-bad-beat");
    let who = updater("d1", "daemon");
    let long = format!(
        r#"{{"platform":"linux-x86_64","version":"1.0","health":"healthy","pad":"{}"}}"#,
        "x".repeat(MAX_HEARTBEAT_BYTES)
    );
    for body in [
        "not json",
        "{}",
        r#"{"platform":"linux-x86_64","version":"-1","health":"healthy"}"#,
        &long,
    ] {
        let (code, text) = call(
            &rig,
            Surface::Heartbeat,
            &who,
            request("POST", HEARTBEAT_PATH, None, body),
        )
        .await;
        assert_eq!(
            code,
            StatusCode::BAD_REQUEST,
            "{}",
            &body[..body.len().min(40)]
        );
        assert_eq!(text, json!({"error": "bad_request"}));
    }
    let broken = Request::builder()
        .method("POST")
        .uri(HEARTBEAT_PATH)
        .body(Broken)
        .expect("a request");
    let response = handle(&rig.coordinator, Surface::Heartbeat, &who, broken, PATIENCE).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(hosts(&status(&rig).await).is_empty());
}

/// Catches: a client that sends a head and then no body holding the request open for ever,
/// on either surface: after the patience the request is a bad request, nothing is noted and
/// no state is set (the control call does not need a token to be held up either).
#[tokio::test(start_paused = true)]
async fn a_body_that_never_arrives_is_a_bad_request() {
    let rig = Rig::new("http-stalled");
    let who = updater("d1", "daemon");
    let patience = std::time::Duration::from_millis(50);
    for (surface, peer, path) in [
        (Surface::Heartbeat, &who, HEARTBEAT_PATH),
        (Surface::Control, &None, DESIRED_PATH),
    ] {
        let stalled = Request::builder()
            .method("POST")
            .uri(path)
            .body(Stalled)
            .expect("a request");
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            handle(&rig.coordinator, surface, peer, stalled, patience),
        )
        .await
        .expect("answered after the patience, not never");
        assert_eq!(answered.status(), StatusCode::BAD_REQUEST, "{path}");
    }
    assert!(hosts(&status(&rig).await).is_empty());
}

struct Stalled;

impl Body for Stalled {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        Poll::Pending
    }
}

struct Broken;

impl Body for Broken {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        Poll::Ready(Some(Err(io::Error::other("the client went away"))))
    }
}

/// Catches: a route answered on the wrong listener or with the wrong method: the control
/// paths on the heartbeat listener and the heartbeat path on the control listener are not
/// found, a known path with another method is 405, an unknown path is 404.
#[tokio::test]
async fn each_listener_serves_its_own_routes() {
    let rig = Rig::new("http-routes");
    let who = updater("d1", "daemon");
    for (surface, method, path, want) in [
        (
            Surface::Heartbeat,
            "POST",
            DESIRED_PATH,
            StatusCode::NOT_FOUND,
        ),
        (
            Surface::Heartbeat,
            "GET",
            STATUS_PATH,
            StatusCode::NOT_FOUND,
        ),
        (
            Surface::Heartbeat,
            "GET",
            HEARTBEAT_PATH,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Surface::Heartbeat,
            "PUT",
            HEARTBEAT_PATH,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Surface::Heartbeat,
            "POST",
            "/v1/heartbeat/",
            StatusCode::NOT_FOUND,
        ),
        (
            Surface::Control,
            "POST",
            HEARTBEAT_PATH,
            StatusCode::NOT_FOUND,
        ),
        (
            Surface::Control,
            "GET",
            DESIRED_PATH,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Surface::Control,
            "POST",
            STATUS_PATH,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Surface::Heartbeat,
            "GET",
            CONVERGENCE_PATH,
            StatusCode::NOT_FOUND,
        ),
        (
            Surface::Control,
            "POST",
            CONVERGENCE_PATH,
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (Surface::Control, "GET", "/", StatusCode::NOT_FOUND),
    ] {
        let (code, body) = call(&rig, surface, &who, request(method, path, None, "")).await;
        assert_eq!(code, want, "{method} {path}");
        assert!(body["error"].is_string());
    }
}

/// Catches: the desired state set with the wrong credential over HTTP: CI's token sets it,
/// the operator token, no token, a token for another repository and a bearer in another
/// scheme are all 401 and say only `unauthorized`, never a claim.
#[tokio::test]
async fn the_desired_state_is_set_by_ci_and_refused_to_anyone_else() {
    let rig = Rig::new("http-set");
    let (code, set) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), STATE),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(set["version"], "2.0");
    assert_eq!(status(&rig).await["desired"]["version"], "2.0");

    let basic = Request::builder()
        .method("POST")
        .uri(DESIRED_PATH)
        .header("authorization", format!("Basic {}", ci_token()))
        .body(Full::new(Bytes::from(STATE)))
        .expect("a request");
    let mut other_repo = kbf_oidc::testkit::good_claims(kbf_oidc::testkit::STATIC_ISSUER);
    other_repo["repository_id"] = json!("1");
    let other_repo = kbf_oidc::testkit::mint(&kbf_oidc::testkit::keys()[0], &other_repo);
    let mut refused = vec![
        call(
            &rig,
            Surface::Control,
            &None,
            request("POST", DESIRED_PATH, Some(OPERATOR), STATE),
        )
        .await,
        call(
            &rig,
            Surface::Control,
            &None,
            request("POST", DESIRED_PATH, None, STATE),
        )
        .await,
        call(
            &rig,
            Surface::Control,
            &None,
            request("POST", DESIRED_PATH, Some(&other_repo), STATE),
        )
        .await,
    ];
    refused.push(
        answer(handle(&rig.coordinator, Surface::Control, &None, basic, PATIENCE).await).await,
    );
    for (code, body) in refused {
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
}

/// Catches: a bad body with a good token answered as success or as a server fault; a body
/// over the cap read in full; a body that fails while read taken for one; an unreachable
/// issuer or a state file that cannot be written told as the caller's fault.
#[tokio::test]
async fn a_bad_state_is_the_callers_fault_and_a_failing_store_is_ours() {
    let rig = Rig::new("http-set-bad");
    let big = format!(
        r#"{{"version":"2.0","pad":"{}"}}"#,
        "x".repeat(MAX_DESIRED_BYTES)
    );
    for body in ["{}", "nope", &big] {
        let (code, text) = call(
            &rig,
            Surface::Control,
            &None,
            request("POST", DESIRED_PATH, Some(&ci_token()), body),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(text, json!({"error": "bad_request"}));
    }
    let broken = Request::builder()
        .method("POST")
        .uri(DESIRED_PATH)
        .body(Broken)
        .expect("a request");
    let response = handle(&rig.coordinator, Surface::Control, &None, broken, PATIENCE).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(status(&rig).await["desired"], Value::Null);

    let unwritable = std::fs::create_dir(rig.dir.path().join("desired.json.new"));
    unwritable.expect("a directory where the temporary file goes");
    let (code, text) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), STATE),
    )
    .await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(text, json!({"error": "internal"}));
}

/// Catches: the status served to anyone but the operator token: 401 without it, with a CI
/// token, and with a wrong one; and with it, 200.
#[tokio::test]
async fn the_status_is_served_to_the_operator_token_only() {
    let rig = Rig::new("http-status");
    for bearer in [None, Some("wrong"), Some(ci_token().as_str())] {
        let (code, body) = call(
            &rig,
            Surface::Control,
            &None,
            request("GET", STATUS_PATH, bearer, ""),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
    assert_eq!(status(&rig).await["quiet"], true, "just started");
    rig.at(1_100);
    assert_eq!(status(&rig).await["quiet"], false);
}

/// Catches: an issuer that cannot be reached answered as a bad token (401), and an
/// Authorization header that is not text taken for a bearer: the first is 503 and the second
/// 401.
#[tokio::test]
async fn an_unreachable_issuer_is_503_and_an_unreadable_header_is_no_bearer() {
    let rig = Rig::with_unreachable_issuer("http-down");
    let (code, body) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), STATE),
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({"error": "unavailable"}));
    let opaque = Request::builder()
        .method("GET")
        .uri(STATUS_PATH)
        .header(
            "authorization",
            hyper::header::HeaderValue::from_bytes(b"Bearer \xff\xfe").expect("a header"),
        )
        .body(Full::new(Bytes::new()))
        .expect("a request");
    let (code, _) =
        answer(handle(&rig.coordinator, Surface::Control, &None, opaque, PATIENCE).await).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

async fn convergence(rig: &Rig, bearer: Option<&str>) -> (StatusCode, Value) {
    call(
        rig,
        Surface::Control,
        &None,
        request("GET", CONVERGENCE_PATH, bearer, ""),
    )
    .await
}

/// Catches: the convergence read not being CI's: over HTTP it is 401 with no bearer, with
/// the operator token and with a token that is no CI token's, 200 with a CI token, and a CI
/// token does not open the operator's status.
#[tokio::test]
async fn the_convergence_needs_a_ci_token_over_http() {
    let rig = Rig::new("http-converge-auth");
    for bearer in [None, Some(OPERATOR), Some("not.a.token")] {
        let (code, body) = convergence(&rig, bearer).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
    let (code, _) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(
        &rig,
        Surface::Control,
        &None,
        request("GET", STATUS_PATH, Some(&ci_token()), ""),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

/// Catches: the shape CI parses drifting, and the three states being misread from heartbeats
/// over HTTP: a farm mid-roll, a farm all at the version and a farm with a failed host read
/// in_progress, converged and blocked, with the counts per role.
#[tokio::test]
async fn the_convergence_reports_in_progress_converged_and_blocked() {
    let rig = Rig::new("http-converge");
    rig.set(STATE).await;
    rig.at(2_000);
    let beat_of = |name: &str, role: &str, body: &str| {
        (
            updater(name, role),
            request("POST", HEARTBEAT_PATH, None, body),
        )
    };
    let at =
        |v: &str| format!(r#"{{"platform":"linux-x86_64","version":"{v}","health":"healthy"}}"#);
    let behind = at("1.0");
    let ok = at("2.0");
    let failed = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy",
        "last_update":{"target":"2.0","outcome":"failed"}}"#
        .to_owned();
    let send = |hosts: Vec<(Peer, Request<Full<Bytes>>)>| async {
        for (who, req) in hosts {
            let (code, _) = call(&rig, Surface::Heartbeat, &who, req).await;
            assert_eq!(code, StatusCode::OK);
        }
    };
    send(vec![
        beat_of("st1", "storage", &ok),
        beat_of("st2", "storage", &behind),
    ])
    .await;
    let (code, mid) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(mid["state"], "in_progress");
    assert_eq!(mid["desired"], "2.0");
    assert_eq!(mid["quiet"], false);
    assert_eq!(
        mid["roles"],
        json!([{"role": "storage", "at_desired": 1, "updating": 0, "failed": 0, "down": 0}])
    );
    send(vec![
        beat_of("st2", "storage", &ok),
        beat_of("fe1", "frontend", &ok),
    ])
    .await;
    let (_, done) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(done["state"], "converged");
    send(vec![beat_of("st2", "storage", &failed)]).await;
    let (_, stuck) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(stuck["state"], "blocked");
    assert_eq!(stuck["roles"][1]["failed"], 1);
    assert_eq!(stuck["roles"][1]["role"], "storage");
}

/// Catches: an issuer that cannot be reached answered as a bad token: the convergence read
/// is 503, as setting the state is.
#[tokio::test]
async fn the_convergence_with_an_unreachable_issuer_is_503() {
    let rig = Rig::with_unreachable_issuer("http-converge-down");
    let (code, body) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({"error": "unavailable"}));
}

/// What a v0.0.18 updater sends: the heartbeat's JSON as it serializes today, with a last
/// update and no update running. The bytes are pinned, not rebuilt from the type.
const GOLDEN_BEAT: &str = concat!(
    r#"{"platform":"linux-x86_64","version":"0.0.18+gabcdefabcdef-2","health":"healthy","#,
    r#""last_update":{"target":"0.0.18+gabcdefabcdef-2","outcome":"succeeded"},"updating":null}"#
);

/// The body `cargo xtask deploy-farm` posts to `/v1/desired` today (keys sorted).
const GOLDEN_DESIRED: &str =
    r#"{"min_up":{"frontend":1,"storage":2},"version":"0.0.19+g123456abcdef-1"}"#;

async fn raw(
    rig: &Rig,
    surface: Surface,
    peer: &Peer,
    req: Request<Full<Bytes>>,
) -> (u16, Vec<u8>) {
    let response = handle(&rig.coordinator, surface, peer, req, PATIENCE).await;
    let code = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (code, bytes.to_vec())
}

/// Pins: the wire of the update path stays as it is. A heartbeat of a v0.0.18 updater is
/// read to the same Heartbeat it was written from, deploy-farm's `POST /v1/desired` body
/// (no `service`) is accepted and echoed as the state without a `service` key, and the
/// updater behind gets exactly the bytes `{"reply":"update","version":...}` while a current
/// one gets exactly `{"reply":"nothing"}`, with the same status line of 200.
#[tokio::test]
async fn a_v0_0_18_updater_and_deploy_farm_get_the_bytes_they_always_got() {
    let rig = Rig::new("http-golden");
    let who = updater("d1", "daemon");
    let (code, set) = raw(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), GOLDEN_DESIRED),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        String::from_utf8(set).expect("text"),
        r#"{"version":"0.0.19+g123456abcdef-1","max_updating":{"frontend":1,"storage":1,"daemon":2,"mac-daemon":2},"min_up":{"storage":2,"frontend":1}}"#
    );
    rig.at(2_000);
    let (code, behind) = raw(
        &rig,
        Surface::Heartbeat,
        &who,
        request("POST", HEARTBEAT_PATH, None, GOLDEN_BEAT),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        String::from_utf8(behind).expect("text"),
        r#"{"reply":"update","version":"0.0.19+g123456abcdef-1"}"#
    );
    let current = GOLDEN_BEAT.replace("0.0.18+gabcdefabcdef-2", "0.0.19+g123456abcdef-1");
    let (code, nothing) = raw(
        &rig,
        Surface::Heartbeat,
        &updater("d2", "daemon"),
        request("POST", HEARTBEAT_PATH, None, &current),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        String::from_utf8(nothing).expect("text"),
        r#"{"reply":"nothing"}"#
    );
}

/// Pins: `service` in the body. `"service":"kbf"` sets the same state as no `service` (the
/// status shows it under `desired` and under `services.kbf`, and a host is told to update),
/// and a service that is not `kbf` is a 400 `bad_request` that changes nothing.
#[tokio::test]
async fn the_desired_call_takes_a_service_and_refuses_an_unknown_one() {
    let rig = Rig::new("http-service");
    let other = r#"{"service":"other","version":"9.0","min_up":{"storage":1,"frontend":1}}"#;
    let (code, body) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), other),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({"error": "bad_request"}));
    let seen = status(&rig).await;
    assert_eq!(seen["desired"], Value::Null);
    assert_eq!(seen["services"], json!({}));
    let named = r#"{"service":"kbf","version":"2.0","min_up":{"storage":1,"frontend":1}}"#;
    let (code, _) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), named),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let seen = status(&rig).await;
    assert_eq!(seen["desired"]["version"], "2.0");
    assert_eq!(seen["services"]["kbf"], seen["desired"]);
    rig.at(2_000);
    let (_, reply) = call(
        &rig,
        Surface::Heartbeat,
        &updater("d1", "daemon"),
        request("POST", HEARTBEAT_PATH, None, BEAT),
    )
    .await;
    assert_eq!(reply, json!({"reply": "update", "version": "2.0"}));
}

/// Pins: a token whose entry may not set the service is a 403 `forbidden` that changes
/// nothing.
#[tokio::test]
async fn a_service_the_token_may_not_set_is_a_403() {
    let mut rig = Rig::new("http-forbidden");
    let list = format!(
        "{}services = [\"other\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list);
    let body = r#"{"version":"2.0","min_up":{"storage":1,"frontend":1}}"#;
    let (code, body) = call(
        &rig,
        Surface::Control,
        &None,
        request("POST", DESIRED_PATH, Some(&ci_token()), body),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));
    assert_eq!(status(&rig).await["desired"], Value::Null);
}

/// Catches: the status or the convergence losing a field a client already reads, or not
/// saying which build answers: a coordinator that has seen no host and holds no desired state
/// answers with exactly the earlier fields and one more, `commit`, the text `--version`
/// shows after the `+`.
#[tokio::test]
async fn the_status_and_the_convergence_say_the_build_commit_and_keep_their_fields() {
    let rig = Rig::new("http-commit");
    let commit = crate::BUILD_COMMIT;
    assert!(
        commit == "unknown"
            || (commit.len() == 12 && commit.bytes().all(|b| b.is_ascii_hexdigit())),
        "{commit:?}"
    );
    assert_eq!(
        status(&rig).await,
        json!({"desired": null, "services": {}, "quiet": true, "services_dropped": 0, "hosts": [], "per_service": {"kbf": {"desired": null, "roles": [], "hosts": [], "up": 0, "slots_held": 0, "convergence": {"desired": null, "quiet": true, "state": "in_progress", "roles": [], "commit": commit}}}, "commit": commit})
    );
    let (code, body) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "desired": null,
            "quiet": true,
            "state": "in_progress",
            "roles": [],
            "commit": commit,
        })
    );
    rig.set(STATE).await;
    let (_, set) = convergence(&rig, Some(&ci_token())).await;
    assert_eq!(set["commit"], commit);
    assert_eq!(set["desired"], "2.0");
}
