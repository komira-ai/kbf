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

use crate::http::{DESIRED_PATH, MAX_DESIRED_BYTES, Peer, STATUS_PATH, Surface, handle};
use crate::testutil::{OPERATOR, Rig, ci_token};

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
    answer(handle(&rig.coordinator, surface, peer, request).await).await
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
    let response = handle(&rig.coordinator, Surface::Heartbeat, &who, broken).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(hosts(&status(&rig).await).is_empty());
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
    refused.push(answer(handle(&rig.coordinator, Surface::Control, &None, basic).await).await);
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
    let response = handle(&rig.coordinator, Surface::Control, &None, broken).await;
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
    let (code, _) = answer(handle(&rig.coordinator, Surface::Control, &None, opaque).await).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}
