//! A CI job's GitHub OIDC token for the two update calls, end to end through the serving core:
//! what reaches the update handler and the child with a good token (the same as with the
//! operator token), and what is refused, with the handler and the child never reached.
//!
//! The issuer is `kbf_oidc::testkit` (RSA keys made in the test, a key source that opens no
//! socket), the time is the verifier's own settable clock, and the children are recording
//! fakes. Each refusal row is a token that differs from a good one in one claim or one byte.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use kbf_oidc::testkit::{
    NOW, STATIC_ISSUER, clock, config_text, good_claims, keys, mint, mint_raw, mint_with,
    static_verifier,
};
use kbf_oidc::{OidcConfig, Verifier};
use serde_json::{Value, json};

use super::kit::{Access, OPERATOR_TOKEN, Rig, Script, read_all, script, security, tagged};
use crate::auth::{Buck2Auth, FAILURE_BURST};
use crate::config::Limits;
use crate::control::{UpdateHandler, UpdateReply, UpdateRequest};
use crate::remote::RemoteCi;
use crate::slot::{ActiveSlot, Slot};

const DESIRED: &str = "/v1/update/desired";
const FRONTEND: &str = "/v1/update/frontend";
const DESIRED_BODY: &[u8] = b"{\"commit\":\"0123456789abcdef0123456789abcdef01234567\"}";
const FRONTEND_BODY: &[u8] = b"{\"commit\":\"0123456789abcdef0123456789abcdef01234567\",\"request_id\":\"r1\"}";

/// What the active child saw of one request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Got {
    method: String,
    uri: String,
    body: Vec<u8>,
    authorization: Option<String>,
}

type ChildLog = Arc<Mutex<Vec<Got>>>;
type HandlerLog = Arc<Mutex<Vec<UpdateRequest>>>;

fn recording_child(log: &ChildLog) -> Script {
    let log = Arc::clone(log);
    script(move |mut call| {
        let log = Arc::clone(&log);
        async move {
            let body = read_all(&mut call.body).await;
            log.lock().unwrap_or_else(PoisonError::into_inner).push(Got {
                method: call.method.to_string(),
                uri: call
                    .uri
                    .path_and_query()
                    .map_or_else(String::new, |p| p.as_str().to_owned()),
                body,
                authorization: call
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            });
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(())
                .expect("a response");
            if let Ok(mut send) = call.respond.send_response(response, false) {
                let _ = send.send_data(Bytes::from_static(b"{\"from\":\"child\"}"), true);
            }
        }
    })
}

struct Handler(HandlerLog);

impl UpdateHandler for Handler {
    async fn handle(&self, request: UpdateRequest) -> UpdateReply {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(request);
        UpdateReply::json(202, b"{\"from\":\"handler\"}".to_vec())
    }
}

/// A proxy with a recording child on A and handler, whose update credential admits OIDC
/// tokens of `verifier` if there is one.
struct Case {
    rig: Rig,
    child: ChildLog,
    handler: HandlerLog,
}

async fn case(remote: Option<Arc<Verifier>>) -> Case {
    let (child, handler): (ChildLog, HandlerLog) = Default::default();
    let slots = ActiveSlot::new();
    slots.open(Slot::A);
    slots.set_active(Slot::A).expect("A is open");
    let mut secured = security(Buck2Auth::None);
    if let Some(verifier) = remote {
        secured = secured.with_remote(RemoteCi::new(verifier));
    }
    let rig = Rig::build_with(
        recording_child(&child),
        tagged("b"),
        Handler(Arc::clone(&handler)),
        Limits::default(),
        slots,
        secured,
        Access::Plain(None),
    )
    .await;
    Case {
        rig,
        child,
        handler,
    }
}

impl Case {
    /// The status of one call carrying `bearer`.
    async fn call(&self, method: &str, uri: &str, body: &'static [u8], bearer: &str) -> u16 {
        let mut client = self.rig.client().await;
        client.bearer = Some(bearer.to_owned());
        client
            .plain(method, uri, body)
            .await
            .status
            .expect("an answer")
    }

    fn reached(&self) -> (Vec<Got>, Vec<UpdateRequest>) {
        (
            self.child.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            self.handler.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        )
    }
}

fn token(claims: &Value) -> String {
    mint(&keys()[0], claims)
}

fn good() -> String {
    token(&good_claims(STATIC_ISSUER))
}

/// A token for `claims` after `edit`.
fn edited(edit: impl FnOnce(&mut Value)) -> String {
    let mut claims = good_claims(STATIC_ISSUER);
    edit(&mut claims);
    token(&claims)
}

/// A good token's call to each of the two routes does what the operator's call does: the same
/// request reaches the child (with the operator token in place of the OIDC token) and the
/// update handler.
///
/// Kills: a route not wired to its handler; the OIDC token passed to the child (its gate
/// takes the operator token only); the body or path changed on the way; an admitted call
/// answered by the proxy instead of the real handler.
#[tokio::test(start_paused = true)]
async fn a_good_token_reaches_the_same_handlers_as_the_operator_token() {
    let (verifier, _) = static_verifier();
    let by_operator = case(Some(Arc::clone(&verifier))).await;
    let by_token = case(Some(verifier)).await;
    for (route, body) in [(DESIRED, DESIRED_BODY), (FRONTEND, FRONTEND_BODY)] {
        let operator = by_operator.call("POST", route, body, OPERATOR_TOKEN).await;
        let oidc = by_token.call("POST", route, body, &good()).await;
        assert_eq!(operator, oidc, "{route}");
        assert!(matches!(oidc, 200 | 202), "{route}: {oidc}");
    }
    let (child, handler) = by_token.reached();
    assert_eq!(child.len(), 1, "{child:?}");
    assert_eq!(handler.len(), 1, "{handler:?}");
    assert_eq!(
        child[0].authorization.as_deref(),
        Some(format!("Bearer {OPERATOR_TOKEN}").as_str()),
        "the child is shown the operator token and never the OIDC token"
    );
    assert_eq!(
        (child, handler),
        by_operator.reached(),
        "the same calls reach the same places"
    );
    assert_eq!(handler_path(&by_token), FRONTEND);
}

fn handler_path(case: &Case) -> String {
    case.reached().1[0].path.clone()
}

/// One token that differs from a good one in a single way is refused with 401, and neither
/// the child nor the handler is reached, on either route.
///
/// Kills: each check of the verifier taken out (a mutant that skips the audience check, one
/// that accepts any `kid`), and a refusal that still forwards the call.
#[tokio::test(start_paused = true)]
async fn a_token_that_is_wrong_in_one_way_is_refused_on_both_routes() {
    let (verifier, now) = static_verifier();
    let other = &keys()[1];
    let wrong: Vec<(&str, String)> = vec![
        ("issuer", edited(|c| c["iss"] = json!("https://evil.example"))),
        ("audience", edited(|c| c["aud"] = json!("someone-else"))),
        ("audience list", edited(|c| c["aud"] = json!(["kbf-gamma", "x"]))),
        ("repository_id", edited(|c| c["repository_id"] = json!("1"))),
        ("repository", edited(|c| c["repository"] = json!("evil/fork"))),
        ("ref", edited(|c| c["ref"] = json!("refs/heads/evil"))),
        ("environment", edited(|c| c["environment"] = json!("prod"))),
        (
            "workflow",
            edited(|c| c["job_workflow_ref"] = json!("komira-ai/komira-build-farm/.github/workflows/evil.yml@refs/heads/main")),
        ),
        ("event", edited(|c| c["event_name"] = json!("pull_request"))),
        ("runner", edited(|c| c["runner_environment"] = json!("self-hosted"))),
        (
            "expired",
            edited(|c| {
                c["exp"] = json!(NOW - 3600);
                c["iat"] = json!(NOW - 3900);
                c["nbf"] = json!(NOW - 3900);
            }),
        ),
        ("not yet valid", edited(|c| c["nbf"] = json!(NOW + 3600))),
        // Signed by a key the issuer does not publish, under the name of one it does.
        (
            "bad signature",
            mint_with(
                other,
                &json!({"alg": "RS256", "typ": "JWT", "kid": "k1"}),
                &good_claims(STATIC_ISSUER),
            ),
        ),
        // A key the issuer publishes under no such name, or none.
        (
            "unknown kid",
            mint_with(
                &keys()[2],
                &json!({"alg": "RS256", "typ": "JWT", "kid": "k3"}),
                &good_claims(STATIC_ISSUER),
            ),
        ),
        // A signature that holds, under a name the issuer does not publish: the key must be
        // found by name, not tried.
        (
            "kid of no key",
            mint_with(
                &keys()[0],
                &json!({"alg": "RS256", "typ": "JWT", "kid": "nope"}),
                &good_claims(STATIC_ISSUER),
            ),
        ),
        (
            "no kid",
            mint_with(
                &keys()[0],
                &json!({"alg": "RS256", "typ": "JWT"}),
                &good_claims(STATIC_ISSUER),
            ),
        ),
        (
            "alg none",
            mint_raw(
                &json!({"alg": "none", "typ": "JWT", "kid": "k1"}),
                &good_claims(STATIC_ISSUER),
                b"",
            ),
        ),
        (
            "alg HS256",
            mint_raw(
                &json!({"alg": "HS256", "typ": "JWT", "kid": "k1"}),
                &good_claims(STATIC_ISSUER),
                b"0123456789abcdef0123456789abcdef",
            ),
        ),
        ("not a token", "aaa.bbb.ccc".to_owned()),
    ];
    let case = case(Some(verifier)).await;
    for (what, bad) in &wrong {
        // The failures of one address are limited: each row is a new connection from the
        // same address, so keep under the burst by refilling with the paused clock.
        tokio::time::advance(std::time::Duration::from_secs(FAILURE_BURST.into())).await;
        for (route, body) in [(DESIRED, DESIRED_BODY), (FRONTEND, FRONTEND_BODY)] {
            assert_eq!(case.call("POST", route, body, bad).await, 401, "{what} {route}");
        }
    }
    assert_eq!(case.reached(), (Vec::new(), Vec::new()), "nothing was reached");
    // The rig is sound: the same verifier admits a good token, and `now` was not moved.
    assert_eq!(now.load(Ordering::SeqCst), NOW);
    assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, &good()).await, 200);
}

/// A token that was used is not accepted again, and an expired one is refused only because
/// of its time.
///
/// Kills: the `jti` not remembered.
#[tokio::test(start_paused = true)]
async fn a_token_is_good_once() {
    let (verifier, _) = static_verifier();
    let case = case(Some(verifier)).await;
    let once = good();
    assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, &once).await, 200);
    assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, &once).await, 401);
    assert_eq!(case.call("POST", FRONTEND, FRONTEND_BODY, &once).await, 401);
    assert_eq!(case.reached().0.len(), 1);
    assert!(case.reached().1.is_empty());
}

/// A good token is the credential of two calls and no other: any other route, or the same
/// path with another method or a query, is refused as a call with no credential is.
///
/// Kills: a route outside the allow-list let in (a token routed to an unlisted path), a
/// prefix match, the method not checked.
#[tokio::test(start_paused = true)]
async fn a_good_token_is_refused_on_every_other_route() {
    let (verifier, _) = static_verifier();
    let case = case(Some(verifier)).await;
    let calls: [(&str, &str, &'static [u8]); 16] = [
        ("GET", "/v1/update/frontend", b""),
        ("GET", "/v1/update/desired", b""),
        ("POST", "/v1/update/frontend/abort", b"{}"),
        ("POST", "/v1/update/frontend/rollback", b"{}"),
        ("POST", "/v1/update/frontend/force-retire", b"{\"reason\":\"x\"}"),
        ("POST", "/v1/update/pause", b"{}"),
        ("POST", "/v1/update/resume", b"{}"),
        ("GET", "/v1/update", b""),
        ("GET", "/v1/update/hosts", b""),
        ("POST", "/v1/update/desired?x=1", DESIRED_BODY),
        ("POST", "/v1/update/frontend?x=1", FRONTEND_BODY),
        ("POST", "/v1/update/desired/", DESIRED_BODY),
        ("POST", "/v1/members/n7", b"{}"),
        ("POST", "/v1/nodes", b"{}"),
        ("GET", "/v1/nodes", b""),
        ("GET", "/metrics", b""),
    ];
    for (method, uri, body) in calls {
        tokio::time::advance(std::time::Duration::from_secs(FAILURE_BURST.into())).await;
        let status = case.call(method, uri, body, &good()).await;
        assert_eq!(status, 401, "{method} {uri}");
    }
    assert_eq!(case.reached(), (Vec::new(), Vec::new()));
    // And the operator token still opens each of them (the rig reaches the child).
    assert_eq!(
        case.call("POST", "/v1/update/pause", b"{}", OPERATOR_TOKEN).await,
        200
    );
}

/// Without an allow-list a proxy admits no OIDC token, whatever its claims: the operator
/// token is the only credential, as before.
///
/// Kills: the gate on when nothing configured it.
#[tokio::test(start_paused = true)]
async fn without_an_allow_list_a_good_token_is_refused() {
    let case = case(None).await;
    for (route, body) in [(DESIRED, DESIRED_BODY), (FRONTEND, FRONTEND_BODY)] {
        assert_eq!(case.call("POST", route, body, &good()).await, 401, "{route}");
    }
    assert_eq!(case.reached(), (Vec::new(), Vec::new()));
    assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, OPERATOR_TOKEN).await, 200);
    assert_eq!(case.call("POST", FRONTEND, FRONTEND_BODY, OPERATOR_TOKEN).await, 202);
}

/// An address that has presented too many wrong tokens is turned away before its token is
/// looked at, so a good token from it is 429, and one from another address is admitted.
///
/// Kills: refused OIDC tokens not counted against the address; the limit asked after the
/// verifier.
#[tokio::test(start_paused = true)]
async fn wrong_tokens_count_against_the_address() {
    let (verifier, _) = static_verifier();
    let case = case(Some(verifier)).await;
    let bad = edited(|c| c["aud"] = json!("someone-else"));
    for _ in 0..FAILURE_BURST {
        assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, &bad).await, 401);
    }
    assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, &good()).await, 429);
    assert_eq!(case.reached(), (Vec::new(), Vec::new()));
    let mut other = case.rig.client_from("192.0.2.77".parse().expect("an ip")).await;
    other.bearer = Some(good());
    assert_eq!(other.plain("POST", DESIRED, DESIRED_BODY).await.status, Some(200));
}

struct NoKeys;

impl kbf_oidc::KeySource for NoKeys {
    fn jwks(&self) -> futures::future::BoxFuture<'_, Result<String, String>> {
        Box::pin(async { Err("unreachable".to_owned()) })
    }
}

/// Keys that cannot be had are a 503, not a verdict on the caller: nothing is reached, and
/// the address is not counted against.
///
/// Kills: a key outage answered 401 (a retrying pipeline reads it as a wrong credential) or
/// counted as a wrong token.
#[tokio::test(start_paused = true)]
async fn an_issuer_that_cannot_be_reached_is_a_503_that_costs_the_caller_nothing() {
    let config = OidcConfig::parse(&config_text(STATIC_ISSUER)).expect("a config");
    let (clock, _) = clock();
    let verifier = Arc::new(Verifier::new(
        config,
        Arc::new(NoKeys),
        std::time::Duration::from_secs(30),
        clock,
    ));
    let case = case(Some(verifier)).await;
    for _ in 0..(FAILURE_BURST + 5) {
        assert_eq!(case.call("POST", DESIRED, DESIRED_BODY, &good()).await, 503);
    }
    assert_eq!(case.reached(), (Vec::new(), Vec::new()));
    assert_eq!(case.rig.core.security.failures.tracked(), 0);
}
