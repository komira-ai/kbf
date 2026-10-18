//! The remote operator routes over a real listener, a fake issuer ([`crate::oidc::testkit`])
//! and a recording sink. Raw HTTP/1 requests, so every header is the test's own.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use kbf_state::ManualClock;
use kbf_state::fake::{Fake, Flaw};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{Outcome, RolloutSink, SinkError, StoreSink, router};
use crate::oidc::Verified;
use crate::oidc::testkit::{FakeIssuer, good_claims, jwks_of, keys, mint, verifier};

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl RolloutSink for Recorder {
    fn offer_desired(&self, commit: &str, _: &Verified) -> Result<Outcome, SinkError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("desired {commit}"));
        Ok(Outcome::Applied)
    }

    fn update_frontend(&self, image: &str, _: &Verified) -> Result<Outcome, SinkError> {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("frontend {image}"));
        Ok(Outcome::Unchanged)
    }
}

struct Rig {
    addr: SocketAddr,
    issuer: FakeIssuer,
    sink: Arc<Recorder>,
}

async fn rig() -> Rig {
    let issuer = FakeIssuer::start(jwks_of(&[&keys()[0]])).await;
    let (verifier, _) = verifier(&issuer);
    let sink = Arc::new(Recorder::default());
    let app = router(verifier, Arc::clone(&sink) as Arc<dyn RolloutSink>);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Rig { addr, issuer, sink }
}

impl Rig {
    fn token(&self) -> String {
        mint(&keys()[0], &good_claims(&self.issuer.url))
    }

    /// One request; the status and the JSON body (`null` when there is none).
    async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: &str,
    ) -> (u16, Value) {
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        let mut tcp = tokio::net::TcpStream::connect(self.addr)
            .await
            .expect("connect");
        tcp.write_all(request.as_bytes()).await.expect("write");
        let mut out = Vec::new();
        let _ = tcp.read_to_end(&mut out).await;
        let text = String::from_utf8_lossy(&out).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = text.split("\r\n\r\n").nth(1).unwrap_or_default();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, token: &str, body: &str) -> (u16, Value) {
        let headers = [
            ("Authorization", format!("Bearer {token}")),
            ("Content-Type", "application/json".to_owned()),
        ];
        self.send("POST", path, &headers, body).await
    }

    fn calls(&self) -> Vec<String> {
        self.sink.calls.lock().expect("calls").clone()
    }
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The route does what it is for: a good token and body reach the sink once, with the
/// commit as sent. Mutants: the route not calling the sink; the body field ignored.
#[tokio::test]
async fn a_good_request_reaches_the_sink() {
    let rig = rig().await;
    let (status, body) = rig
        .post(
            "/v1/update/desired",
            &rig.token(),
            &json!({"commit": COMMIT}).to_string(),
        )
        .await;
    assert_eq!(
        (status, &body["status"]),
        (200, &json!("applied")),
        "{body}"
    );
    assert_eq!(rig.calls(), [format!("desired {COMMIT}")]);
}

/// The frontend route names the image from the configured repository and the digest, and
/// nothing the caller sent but the digest.
#[tokio::test]
async fn the_frontend_route_builds_the_image_from_the_configured_repository() {
    let rig = rig().await;
    let digest = format!("sha256:{}", "ab".repeat(32));
    let (status, body) = rig
        .post(
            "/v1/update/frontend",
            &rig.token(),
            &json!({"image_digest": digest}).to_string(),
        )
        .await;
    assert_eq!(
        (status, &body["status"]),
        (200, &json!("unchanged")),
        "{body}"
    );
    assert_eq!(
        rig.calls(),
        [format!("frontend registry.example/kbf/frontend@{digest}")]
    );
}

/// Every refusal leaves the sink untouched, with the status that names it. The table is
/// the point: an unauthenticated route, a browser, a token of the wrong shape, a body that
/// asks for more than a release. Mutants: the `Origin` gate removed; the Bearer parse
/// accepting any scheme; the verifier call removed (every 401 row turns into a 200);
/// `deny_unknown_fields` removed (the strategy row).
#[tokio::test]
async fn refused_requests_never_reach_the_sink() {
    let rig = rig().await;
    let ok = json!({"commit": COMMIT}).to_string();
    let json_type = ("Content-Type", "application/json".to_owned());
    let bearer = |t: &str| ("Authorization", format!("Bearer {t}"));
    let path = "/v1/update/desired";

    let (s, _) = rig
        .send("POST", path, std::slice::from_ref(&json_type), &ok)
        .await;
    assert_eq!(s, 401, "no Authorization");
    let (s, _) = rig
        .send(
            "POST",
            path,
            &[
                json_type.clone(),
                ("Authorization", format!("Basic {}", rig.token())),
            ],
            &ok,
        )
        .await;
    assert_eq!(s, 401, "not a Bearer");
    let (s, b) = rig
        .send(
            "POST",
            path,
            &[json_type.clone(), bearer("not.a.token")],
            &ok,
        )
        .await;
    assert_eq!((s, &b["error"]), (401, &json!("token refused: malformed")));
    let (s, _) = rig
        .send(
            "POST",
            path,
            &[
                json_type.clone(),
                bearer(&rig.token()),
                ("Origin", "https://evil.example".into()),
            ],
            &ok,
        )
        .await;
    assert_eq!(s, 403, "a browser page");
    let (s, _) = rig
        .send(
            "POST",
            path,
            &[("Content-Type", "text/plain".into()), bearer(&rig.token())],
            &ok,
        )
        .await;
    assert_eq!(s, 415, "not JSON");
    let (s, _) = rig.send("POST", path, &[bearer(&rig.token())], &ok).await;
    assert_eq!(s, 415, "no content type");

    for (body, why) in [
        (
            json!({"commit": COMMIT, "strategy": "all-at-once"}).to_string(),
            "a strategy",
        ),
        (
            json!({"commit": COMMIT, "paused": false}).to_string(),
            "another field",
        ),
        (json!({}).to_string(), "no commit"),
        (json!({"commit": "main"}).to_string(), "a branch name"),
        (
            json!({"commit": COMMIT.to_uppercase()}).to_string(),
            "upper case",
        ),
        (json!({"commit": &COMMIT[..39]}).to_string(), "a short hash"),
        ("not json".to_owned(), "not JSON"),
        (String::new(), "empty"),
    ] {
        let (s, _) = rig.post(path, &rig.token(), &body).await;
        assert_eq!(s, 400, "{why}");
    }
    for (body, why) in [
        (
            json!({"image_digest": format!("sha256:{}", "AB".repeat(32))}),
            "upper case",
        ),
        (json!({"image_digest": "sha256:abcd"}), "short"),
        (json!({"image_digest": "latest"}), "a tag"),
        (
            json!({"image_digest": format!("md5:{}", "ab".repeat(32))}),
            "another algorithm",
        ),
        (
            json!({"image_digest": format!("sha256:{}", "ab".repeat(32)), "image": "evil.example/x"}),
            "an image",
        ),
        (json!({"image": "evil.example/x@sha256:ab"}), "no digest"),
    ] {
        let (s, _) = rig
            .post("/v1/update/frontend", &rig.token(), &body.to_string())
            .await;
        assert_eq!(s, 400, "{why}");
    }
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// The listener serves these two routes and nothing else: no read, no admin write.
/// Every operator route, with a good token, is a 404; a GET of the update route is a 405.
/// Mutant: the remote router merged with the operator API's.
#[tokio::test]
async fn nothing_but_the_two_routes_is_served() {
    let rig = rig().await;
    for (method, path) in [
        ("GET", "/v1/nodes"),
        ("GET", "/v1/members"),
        ("GET", "/healthz"),
        ("GET", "/readyz"),
        ("GET", "/metrics"),
        ("GET", "/v1/frontend/drain"),
        ("POST", "/v1/frontend/drain"),
        ("POST", "/v1/nodes/n1:drain"),
        ("POST", "/v1/members/1"),
        ("POST", "/v1/denied"),
        ("DELETE", "/v1/denied?entry=node%20x"),
        ("POST", "/v1/enroll/tokens"),
        ("POST", "/v1/update"),
        ("POST", "/v1/update/desired/"),
    ] {
        let headers = [
            ("Authorization", format!("Bearer {}", rig.token())),
            ("Content-Type", "application/json".to_owned()),
        ];
        let (s, _) = rig.send(method, path, &headers, "{}").await;
        assert_eq!(s, 404, "{method} {path}");
    }
    for method in ["GET", "PUT", "DELETE"] {
        let (s, _) = rig.send(method, "/v1/update/desired", &[], "").await;
        assert_eq!(s, 405, "{method}");
    }
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// A token is good for one request: the same token again is a 401 naming the replay, and
/// the sink saw one call. Mutant: the `jti` memory removed (the route is not idempotent
/// by itself, a replay would re-offer).
#[tokio::test]
async fn a_token_is_good_for_one_request() {
    let rig = rig().await;
    let token = rig.token();
    let body = json!({"commit": COMMIT}).to_string();
    assert_eq!(rig.post("/v1/update/desired", &token, &body).await.0, 200);
    let (s, b) = rig.post("/v1/update/desired", &token, &body).await;
    assert_eq!((s, &b["error"]), (401, &json!("token refused: replayed")));
    assert_eq!(rig.calls().len(), 1);
}

/// A verified token whose body is then refused is spent: the caller mints another.
#[tokio::test]
async fn a_verified_token_with_a_bad_body_is_spent() {
    let rig = rig().await;
    let token = rig.token();
    assert_eq!(rig.post("/v1/update/desired", &token, "{}").await.0, 400);
    let (s, b) = rig
        .post(
            "/v1/update/desired",
            &token,
            &json!({"commit": COMMIT}).to_string(),
        )
        .await;
    assert_eq!((s, &b["error"]), (401, &json!("token refused: replayed")));
}

/// An issuer that cannot be reached, with no keys held, is a 503 (retry), not a 401.
#[tokio::test]
async fn unreachable_keys_are_a_503() {
    let rig = rig().await;
    rig.issuer.set_fail(true);
    let (s, _) = rig
        .post(
            "/v1/update/desired",
            &rig.token(),
            &json!({"commit": COMMIT}).to_string(),
        )
        .await;
    assert_eq!(s, 503);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// A body over the limit is a 413 before anything is parsed.
#[tokio::test]
async fn an_oversized_body_is_refused() {
    let rig = rig().await;
    let big = format!("{{\"commit\": \"{}\"}}", "a".repeat(super::MAX_BODY_BYTES));
    let (s, _) = rig.post("/v1/update/desired", &rig.token(), &big).await;
    assert_eq!(s, 413);
    assert_eq!(rig.calls(), Vec::<String>::new());
}

/// The store-backed sink offers a release once, says so when it is offered again, and
/// applies a changed one; it cannot apply a frontend image yet and says so.
#[test]
fn the_store_sink_offers_through_the_update_rows() {
    let store = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let sink = StoreSink::new(store, Arc::new(|| kbf_types::FarmTime::from_millis(5)));
    let who = Verified {
        jti: "j".into(),
        job_workflow_ref: "w".into(),
        git_ref: None,
        sha: None,
    };
    assert_eq!(sink.offer_desired(COMMIT, &who), Ok(Outcome::Applied));
    assert_eq!(sink.offer_desired(COMMIT, &who), Ok(Outcome::Unchanged));
    let other = "f".repeat(40);
    assert_eq!(sink.offer_desired(&other, &who), Ok(Outcome::Applied));
    assert!(matches!(
        sink.update_frontend("r@sha256:x", &who),
        Err(SinkError::NotConfigured(_))
    ));
}
