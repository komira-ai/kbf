//! The job service's completion calls through the proxy: `/js-done/<reference>` is open (no
//! certificate, no token) to POST only, reaches a member's operator listener whole, and its
//! being open opens nothing else.

use std::sync::{Arc, Mutex, PoisonError};

use super::kit::{Script, read_all, script};
use super::pool_kit::Rig;

type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// A child that records each request's method and target and answers 204, as the frontend
/// answers a call that verifies.
fn receiver(seen: &Seen) -> Script {
    let seen = Arc::clone(seen);
    script(move |mut call| {
        let seen = Arc::clone(&seen);
        async move {
            read_all(&mut call.body).await;
            let target = call
                .uri
                .path_and_query()
                .map_or_else(String::new, |p| p.as_str().to_owned());
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((call.method.to_string(), target));
            let response = http::Response::builder()
                .status(204)
                .body(())
                .expect("a response");
            let _ = call.respond.send_response(response, true);
        }
    })
}

fn recorded(seen: &Seen) -> Vec<(String, String)> {
    seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Asserts that a POST with no credential to `/js-done/<reference>` reaches the child whole
/// (the method, the path and its query) and that the child's 204 comes back. Catches the
/// route behind the operator token, which the job service has none of, and a path changed on
/// the way.
#[tokio::test(start_paused = true)]
async fn a_completion_call_is_open_and_reaches_the_child_whole() {
    let seen = Seen::default();
    let rig = Rig::start(&[receiver(&seen)]);
    let mut client = rig.client().await;
    client.bearer = None;
    let targets = ["/js-done/0123456789abcdef", "/js-done/ref-2?x=1"];
    for target in targets {
        let reply = client.plain("POST", target, b"").await;
        assert_eq!(reply.status, Some(204), "{target}");
    }
    let want: Vec<_> = targets.map(|t| ("POST".to_owned(), t.to_owned())).to_vec();
    assert_eq!(recorded(&seen), want);
}

/// Asserts that every method but POST is a 405 at the proxy that names POST, with or without
/// a body, and that the child is never asked. Catches a read or another write forwarded to
/// the child through the open class, and a 405 without `Allow`.
#[tokio::test(start_paused = true)]
async fn a_completion_call_that_is_not_a_post_is_refused_at_the_proxy() {
    let seen = Seen::default();
    let rig = Rig::start(&[receiver(&seen)]);
    let mut client = rig.client().await;
    client.bearer = None;
    for method in ["GET", "HEAD", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        let reply = client.plain(method, "/js-done/ref-1", b"").await;
        assert_eq!(reply.status, Some(405), "{method}");
        let allow = reply.headers.get("allow").expect("Allow");
        assert_eq!(allow, "POST", "{method}");
    }
    let with_body = client.plain("PUT", "/js-done/ref-1", b"x").await;
    assert_eq!(with_body.status, Some(405));
    assert_eq!(rig.a().state.dispatches(), 0);
    assert!(recorded(&seen).is_empty());
}

/// Asserts that the open route opens nothing else: a POST with no credential to a path that
/// is not one plain segment under `/js-done/` is a 404 and never reaches the child, and the
/// operator API beside it still refuses a caller with no token (401). Catches
/// `/js-done/../v1/nodes` forwarded under the open class, and the open class leaking to `/v1`.
#[tokio::test(start_paused = true)]
async fn a_completion_call_path_cannot_reach_another_route() {
    let seen = Seen::default();
    let rig = Rig::start(&[receiver(&seen)]);
    let mut client = rig.client().await;
    client.bearer = None;
    for target in [
        "/js-done",
        "/js-done/",
        "/js-done/../v1/nodes",
        "/js-done/a/b",
        "/js-done/a%2fb",
        "/js-donex/a",
    ] {
        let reply = client.plain("POST", target, b"").await;
        assert_eq!(reply.status, Some(404), "{target}");
    }
    assert!(recorded(&seen).is_empty());
    for target in ["/v1/status", "/v1/js-done/a"] {
        let reply = client.plain("POST", target, b"").await;
        assert_eq!(reply.status, Some(401), "{target}");
    }
    assert!(recorded(&seen).is_empty());
}
