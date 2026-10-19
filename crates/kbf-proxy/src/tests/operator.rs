//! The operator API through the proxy, over HTTP: every `/v1/*` path but `/v1/update/frontend*`
//! reaches the active child's operator listener with its method, path, query, body and
//! credential intact, and the child's answer comes back. These pin the routing table of
//! `docs/design/frontend-proxy.md` section 3 at the wire, not only in `route_of`.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

use super::kit::{OPERATOR_TOKEN, Rig, Script, read_all, script, tagged};

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
async fn operator_calls_reach_the_active_child_whole_and_the_reply_comes_back() {
    let (la, lb): (Log, Log) = Default::default();
    let rig = Rig::start(recording("a", &la), recording("b", &lb)).await;
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
    assert_eq!(seen(&la), want);
    assert!(seen(&lb).is_empty(), "the inactive slot sees nothing");
}

/// The operator token goes on to the child (its gate decides writes) next to the proxy's
/// secret.
///
/// Kills: `Authorization` stripped on the way to the child; no proxy secret set.
#[tokio::test(start_paused = true)]
async fn the_operator_token_and_the_proxy_secret_reach_the_child() {
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    let mut client = rig.client().await;
    let reply = client.plain("POST", "/v1/members/n7", b"{}").await;
    assert_eq!(reply.status, Some(200));
    let headers = rig.a.state.headers_of("/v1/members/n7");
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
    let (la, lb): (Log, Log) = Default::default();
    let rig = Rig::start(recording("a", &la), recording("b", &lb)).await;
    let mut client = rig.client().await;
    for (method, path) in [
        ("GET", "/v1/update/frontend"),
        ("POST", "/v1/update/frontend/abort"),
    ] {
        let reply = client.plain(method, path, b"").await;
        assert_ne!(
            reply.headers.get("x-child-reply"),
            Some(&http::HeaderValue::from_static("a")),
            "{path} was answered by the child"
        );
    }
    assert_eq!(rig.a.state.dispatches(), 0, "nothing reached the child");
    let near = client.plain("GET", "/v1/update/frontendx", b"").await;
    assert_eq!(near.status, Some(200));
    assert_eq!(
        seen(&la).len(),
        1,
        "a path beside the prefix is the child's"
    );
}

/// After the slot flips, operator calls go to the new active child.
///
/// Kills: a route bound to the slot that was active at start; a flip that leaves operator
/// calls on the old child.
#[tokio::test(start_paused = true)]
async fn operator_calls_follow_the_active_slot_after_a_flip() {
    let (la, lb): (Log, Log) = Default::default();
    let rig = Rig::start(recording("a", &la), recording("b", &lb)).await;
    let mut client = rig.client().await;
    assert_eq!(
        client.plain("GET", "/v1/nodes", b"").await.text(),
        "{\"from\":\"a\"}"
    );
    rig.flip_to_b();
    let after = client.plain("GET", "/v1/nodes", b"").await;
    assert_eq!(after.text(), "{\"from\":\"b\"}");
    assert_eq!(seen(&la).len(), 1);
    assert_eq!(seen(&lb).len(), 1);
}

/// With no active child an operator call is answered 503 by the proxy, plain HTTP with a
/// retry hint, and no child is asked.
///
/// Kills: a 200 or a hang with no active slot; a call sent to a closed slot.
#[tokio::test(start_paused = true)]
async fn operator_calls_with_no_active_child_are_503() {
    let (la, lb): (Log, Log) = Default::default();
    let rig = Rig::start(recording("a", &la), recording("b", &lb)).await;
    rig.core.slots.clear_active();
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
    assert!(seen(&la).is_empty() && seen(&lb).is_empty());
}

/// A child whose `/metrics` answers `status` with `body`, `size` bytes of it when `size` is
/// set; or never answers when `silent`.
fn metrics_child(status: u16, body: &'static str, size: usize, silent: bool) -> Script {
    script(move |mut call| async move {
        read_all(&mut call.body).await;
        if silent {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            return;
        }
        let data = if size > 0 {
            Bytes::from(vec![b'x'; size])
        } else {
            Bytes::from_static(body.as_bytes())
        };
        let response = http::Response::builder()
            .status(status)
            .header("content-type", "text/plain; version=0.0.4")
            .body(())
            .expect("a response");
        if let Ok(mut send) = call.respond.send_response(response, false) {
            let _ = send.send_data(data, true);
        }
    })
}

async fn scrape(rig: &Rig) -> String {
    let mut client = rig.client().await;
    let reply = client.plain("GET", "/metrics", b"").await;
    assert_eq!(reply.status, Some(200));
    reply.text()
}

/// `/metrics` is the proxy's series followed by the active child's, so one scrape of the
/// public port sees both. The child is asked as an operator call: the caller's token and the
/// proxy secret go with it.
///
/// Kills: the child's body not appended; the child asked without the secret; the proxy's own
/// series lost.
#[tokio::test(start_paused = true)]
async fn metrics_carry_the_active_childs_series_after_the_proxys() {
    let child = metrics_child(200, "kbf_child_series_total 7\n", 0, false);
    let rig = Rig::start(child, tagged("b")).await;
    let text = scrape(&rig).await;
    let own = text.find("kbf_proxy_panics_total 0").expect("proxy series");
    let theirs = text
        .find("kbf_child_series_total 7\n")
        .expect("child series");
    assert!(own < theirs, "the proxy's series come first:\n{text}");
    assert!(text.contains("kbf_proxy_child_metrics_failed_total 0"));
    let asked = rig.a.state.headers_of("/metrics");
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].get("authorization").and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {OPERATOR_TOKEN}").as_str())
    );
    assert!(asked[0].contains_key("x-kbf-proxy-auth"));
}

/// After a flip the new active child's series are the ones shown.
///
/// Kills: the metrics fetch bound to a fixed slot.
#[tokio::test(start_paused = true)]
async fn metrics_follow_the_active_slot() {
    let a = metrics_child(200, "kbf_child_series_total 1\n", 0, false);
    let b = metrics_child(200, "kbf_child_series_total 2\n", 0, false);
    let rig = Rig::start(a, b).await;
    rig.flip_to_b();
    let text = scrape(&rig).await;
    assert!(text.contains("kbf_child_series_total 2\n"), "{text}");
    assert!(!text.contains("kbf_child_series_total 1\n"));
}

/// Whatever goes wrong with the child, the proxy's own series are served whole and the
/// failure is counted.
///
/// Kills: a child failure that fails or empties the scrape; an uncounted failure.
#[tokio::test(start_paused = true)]
async fn a_child_that_cannot_give_metrics_leaves_the_proxys_series_and_a_count() {
    let cases: [(&str, Script); 3] = [
        ("an error status", metrics_child(500, "boom\n", 0, false)),
        ("a body over 4 MiB", metrics_child(200, "", 5 << 20, false)),
        ("no answer in 2 s", metrics_child(200, "", 0, true)),
    ];
    for (what, child) in cases {
        let rig = Rig::start(child, tagged("b")).await;
        let text = scrape(&rig).await;
        assert!(text.contains("kbf_proxy_panics_total 0"), "{what}");
        assert!(!text.contains("boom"), "{what}");
        assert!(!text.contains("xxxx"), "{what}");
        assert!(
            text.contains("kbf_proxy_child_metrics_failed_total 1"),
            "{what}:\n{text}"
        );
    }
    let rig = Rig::start(tagged("a"), tagged("b")).await;
    rig.core.slots.clear_active();
    let text = scrape(&rig).await;
    assert!(
        text.contains("kbf_proxy_child_metrics_failed_total 1"),
        "no active child:\n{text}"
    );
}
