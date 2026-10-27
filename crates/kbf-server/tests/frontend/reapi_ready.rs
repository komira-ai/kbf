//! `GET /readyz/reapi` on a started frontend: `/readyz`, and a connected worker. The
//! worker is a fake daemon on the frontend's worker listener; no test here depends on
//! how long anything takes.

use support::{FakeDaemon, hello};

use super::*;

/// A frontend over a storage node, with the node that keeps it ready.
async fn ready_front(more: &[&str]) -> (Node, Front) {
    let node = Node::start().await;
    let url = format!("--storage=http://{}", node.addr);
    // The interval the fake daemon expects in its Welcome.
    let mut argv = vec![url.as_str(), "--heartbeat-interval-ms=100"];
    argv.extend_from_slice(more);
    let front = Front::start(&flags(&argv).expect("flags")).await;
    (node, front)
}

/// `/readyz/reapi` until it answers `code`.
async fn reapi_until(api: SocketAddr, code: u16) -> Value {
    for _ in 0..500 {
        let (got, body) = get(api, "/readyz/reapi").await;
        if got == code {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("/readyz/reapi never answered {code}");
}

/// Catches the outage of a frontend that is ready and has no worker, which kept taking
/// Execute calls it could not place: `/readyz/reapi` is 503 and names `no_worker` while no
/// worker is connected (defect: the route does not exist, or answers `/readyz`'s 200), and
/// `/readyz` stays 200 meanwhile (defect: the worker and enrollment clusters, which ask it,
/// would never get a first worker through). Then 200, with `/readyz`'s body, once a worker
/// connects (defect: a route that stays 503, or one that ignores the worker), and 503 again
/// when its stream ends (defect: counting every node that ever registered, which
/// `GET /v1/nodes` keeps listing as disconnected).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapi_readiness_needs_a_connected_worker() {
    let (_node, front) = ready_front(&[]).await;

    let (code, body) = get(front.api, "/readyz/reapi").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["ready"], false, "{body}");
    assert_eq!(failing(&body), ["no_worker"], "{body}");
    assert_eq!(
        body["failing"][0]["reason"], "no worker is connected to this server",
        "{body}"
    );
    let (code, plain) = get(front.api, "/readyz").await;
    assert_eq!(
        (code, plain["ready"].as_bool()),
        (200, Some(true)),
        "{plain}"
    );

    let daemon = FakeDaemon::connect(front.worker, hello("node-a", 4, 8))
        .await
        .expect("registered");
    let (code, body) = get(front.api, "/readyz/reapi").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, get(front.api, "/readyz").await.1, "the same body");
    assert_eq!(body["ready"], true, "{body}");

    daemon.close();
    let body = reapi_until(front.api, 503).await;
    assert_eq!(failing(&body), ["no_worker"], "{body}");
    let (_, nodes) = get(front.api, "/v1/nodes").await;
    assert_eq!(nodes["nodes"][0]["connected"], false, "{nodes}");
}

/// Catches `/readyz/reapi` dropping `/readyz`'s own checks: with a worker connected, a
/// frontend whose storage node is gone is still 503, naming `storage` and not `no_worker`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapi_readiness_keeps_the_checks_of_readyz() {
    let (mut node, front) = ready_front(&["--readyz-store-timeout-ms=500"]).await;
    let _daemon = FakeDaemon::connect(front.worker, hello("node-a", 4, 8))
        .await
        .expect("registered");
    assert_eq!(get(front.api, "/readyz/reapi").await.0, 200);

    node.stop().await;
    let (code, body) = get(front.api, "/readyz/reapi").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["storage"], "{body}");
}

/// Catches a token gate on the route: a probe has no token, so a frontend that has one
/// still answers `/readyz/reapi` to a request without it (`get` sends none), with both
/// answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapi_readiness_is_open_like_readyz() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch_dir("frontend-reapi-ready");
    let token_file = dir.join("token");
    std::fs::write(&token_file, "kbf-test-token-0123456789abcdef0123456789\n").expect("token");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let flag = format!("--api-token-file={}", token_file.display());
    let (_node, front) = ready_front(&[&flag]).await;
    assert_eq!(get(front.api, "/readyz/reapi").await.0, 503);
    let _daemon = FakeDaemon::connect(front.worker, hello("node-a", 4, 8))
        .await
        .expect("registered");
    assert_eq!(get(front.api, "/readyz/reapi").await.0, 200);
}
