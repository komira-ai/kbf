//! The operator API through the proxy, over HTTP: every `/v1/*` path but `/v1/update/frontend*`
//! reaches a member's operator listener with its method, path, query, body and credential
//! intact, and the member's answer comes back. These pin the routing table of
//! `docs/design/frontend-proxy.md` section 3 at the wire, not only in `route_of`.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

use super::kit::{OPERATOR_TOKEN, Script, read_all, script, tagged};
use super::pool_kit::Rig;

/// One request as a recording child saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Got {
    method: String,
    uri: String,
    body: Vec<u8>,
}

type Log = Arc<Mutex<Vec<Got>>>;

/// A child that records each request's head and body and answers 200 JSON naming `tag`,
/// with a header of its own.
fn recording(tag: &'static str, log: &Log) -> Script {
    let log = Arc::clone(log);
    script(move |mut call| {
        let log = Arc::clone(&log);
        async move {
            let body = read_all(&mut call.body).await;
            let got = Got {
                method: call.method.to_string(),
                uri: call
                    .uri
                    .path_and_query()
                    .map_or_else(String::new, |p| p.as_str().to_owned()),
                body,
            };
            log.lock().unwrap_or_else(PoisonError::into_inner).push(got);
            let response = http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .header("x-child-reply", tag)
                .body(())
                .expect("a response");
            if let Ok(mut send) = call.respond.send_response(response, false) {
                let _ = send.send_data(Bytes::from(format!("{{\"from\":\"{tag}\"}}")), true);
            }
        }
    })
}

fn seen(log: &Log) -> Vec<Got> {
    log.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The calls the gates make, and the update paths that go to the child.
///
/// Kills: a path of the table answered by the proxy; a query string dropped; a method or
/// body changed on the way; the reply not carried back (status, header, body).
#[tokio::test(start_paused = true)]
async fn operator_calls_reach_a_member_whole_and_the_reply_comes_back() {
    let log = Log::default();
    let rig = Rig::start(&[recording("a", &log)]);
    let mut client = rig.client().await;
    let calls: [(&str, &str, &'static [u8]); 6] = [
        ("GET", "/v1/nodes?state=up&limit=5", b""),
        ("POST", "/v1/members/n7?reason=drain", b"{\"cordon\":true}"),
        ("POST", "/v1/update/desired", b"{\"commit\":\"abc\"}"),
        ("POST", "/v1/update/pause", b"{}"),
        ("POST", "/v1/update/resume", b"{}"),
        ("GET", "/v1/update", b""),
    ];
    for (method, uri, body) in calls {
        let reply = client.plain(method, uri, body).await;
        assert_eq!(reply.status, Some(200), "{method} {uri}");
        assert_eq!(reply.text(), "{\"from\":\"a\"}", "{method} {uri}");
        assert_eq!(
            reply
                .headers
                .get("x-child-reply")
                .and_then(|v| v.to_str().ok()),
            Some("a"),
            "the child's response headers come back"
        );
    }
    let want: Vec<Got> = calls
        .iter()
        .map(|(m, u, b)| Got {
            method: (*m).to_owned(),
            uri: (*u).to_owned(),
            body: b.to_vec(),
        })
        .collect();
    assert_eq!(seen(&log), want);
}

/// The operator token goes on to the child (its gate decides writes) next to the proxy's
/// secret.
///
/// Kills: `Authorization` stripped on the way to the child; no proxy secret set.
#[tokio::test(start_paused = true)]
async fn the_operator_token_and_the_proxy_secret_reach_the_child() {
    let rig = Rig::start(&[tagged("a")]);
    let mut client = rig.client().await;
    let reply = client.plain("POST", "/v1/members/n7", b"{}").await;
    assert_eq!(reply.status, Some(200));
    let headers = rig.a().state.headers_of("/v1/members/n7");
    assert_eq!(headers.len(), 1);
    assert_eq!(
        headers[0]
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {OPERATOR_TOKEN}").as_str())
    );
    assert!(headers[0].contains_key("x-kbf-proxy-auth"));
}

/// `/v1/update/frontend*` is the proxy's, and never goes to a child.
///
/// Kills: the update prefix forwarded; `/v1/update/frontendx` taken for the update API.
#[tokio::test(start_paused = true)]
async fn the_frontend_update_api_is_served_by_the_proxy_not_forwarded() {
    let log = Log::default();
    let rig = Rig::start(&[recording("a", &log)]);
    let mut client = rig.client().await;
    for (method, path) in [
        ("GET", "/v1/update/frontend"),
        ("POST", "/v1/update/frontend/abort"),
    ] {
        let reply = client.plain(method, path, b"").await;
        assert_eq!(reply.status, Some(503), "{path}: the built-in handler");
        assert_ne!(
            reply.headers.get("x-child-reply"),
            Some(&http::HeaderValue::from_static("a")),
            "{path} was answered by the child"
        );
    }
    assert_eq!(rig.dispatches(), [0], "nothing reached the child");
    let near = client.plain("GET", "/v1/update/frontendx", b"").await;
    assert_eq!(near.status, Some(200));
    assert_eq!(
        seen(&log).len(),
        1,
        "a path beside the prefix is the child's"
    );
}

/// Asserts that operator calls go to the members that take streams: the first call is answered
/// by `a`, and once `a` drains the other member has all the calls that follow.
#[tokio::test(start_paused = true)]
async fn operator_calls_follow_the_members_that_take_streams() {
    let (la, lb): (Log, Log) = Default::default();
    let rig = Rig::start(&[recording("a", &la), recording("b", &lb)]);
    let mut client = rig.client().await;
    assert_eq!(
        client.plain("GET", "/v1/nodes", b"").await.text(),
        "{\"from\":\"a\"}"
    );
    assert!(rig.members.set_draining("a", true));
    for _ in 0..2 {
        let after = client.plain("GET", "/v1/nodes", b"").await;
        assert_eq!(after.text(), "{\"from\":\"b\"}");
    }
    assert_eq!(seen(&la).len(), 1);
    assert_eq!(seen(&lb).len(), 2);
}

/// Asserts that with no member taking streams an operator call is answered 503 by the proxy,
/// plain HTTP with a retry hint, and that no child is asked.
#[tokio::test(start_paused = true)]
async fn operator_calls_with_no_member_taking_streams_are_503() {
    let (la, lb): (Log, Log) = Default::default();
    let rig = Rig::start(&[recording("a", &la), recording("b", &lb)]);
    assert!(rig.members.set_draining("a", true));
    assert!(rig.members.set_draining("b", true));
    let mut client = rig.client().await;
    let reply = client.plain("GET", "/v1/nodes", b"").await;
    assert_eq!(reply.status, Some(503));
    assert_eq!(reply.text(), "no frontend is active");
    assert_eq!(
        reply
            .headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert!(seen(&la).is_empty());
    assert!(seen(&lb).is_empty());
}
