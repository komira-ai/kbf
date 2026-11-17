//! `GET /readyz/reapi` on a started frontend: `/readyz`, and a connected worker. The
//! worker is a fake daemon on the frontend's worker listener; no test here depends on
//! how long anything takes.

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

/// `/readyz/reapi` until it names the failing check `check`.
async fn reapi_until_failing(api: SocketAddr, check: &str) -> Value {
    for _ in 0..500 {
        let (_, body) = get(api, "/readyz/reapi").await;
        if failing(&body).contains(&check) {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("/readyz/reapi never named {check}");
}

/// Catches a frontend that is ready and runs no executor, which kept taking Execute calls it
/// could answer only from the action cache: `/readyz/reapi` is 503 and names `no_executor`
/// (defect: the route does not exist, or answers `/readyz`'s 200), and `/readyz` stays 200
/// meanwhile (defect: the clusters that ask it would never get a first frontend through).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapi_readiness_needs_an_executor() {
    let (_node, front) = ready_front(&[]).await;

    let (code, body) = get(front.api, "/readyz/reapi").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["ready"], false, "{body}");
    assert_eq!(failing(&body), ["no_executor"], "{body}");
    assert_eq!(
        body["failing"][0]["reason"], "this server runs no executor (--executor js-agent)",
        "{body}"
    );
    let (code, plain) = get(front.api, "/readyz").await;
    assert_eq!(
        (code, plain["ready"].as_bool()),
        (200, Some(true)),
        "{plain}"
    );
}

/// Catches `/readyz/reapi` dropping `/readyz`'s own checks: a frontend whose storage node is
/// gone is 503 naming `storage` as well as `no_executor`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapi_readiness_keeps_the_checks_of_readyz() {
    let (mut node, front) = ready_front(&["--readyz-store-timeout-ms=500"]).await;
    assert_eq!(
        failing(&get(front.api, "/readyz/reapi").await.1),
        ["no_executor"]
    );

    node.stop().await;
    let body = reapi_until_failing(front.api, "storage").await;
    assert_eq!(failing(&body), ["storage", "no_executor"], "{body}");
}

/// Catches a token gate on the route: a probe has no token, so a frontend that has one
/// still answers `/readyz/reapi` to a request without it (`get` sends none), with its
/// readiness and not a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapi_readiness_is_open_like_readyz() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch_dir("frontend-reapi-ready");
    let token_file = dir.join("token");
    std::fs::write(&token_file, "kbf-test-token-0123456789abcdef0123456789\n").expect("token");
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let flag = format!("--api-token-file={}", token_file.display());
    let (_node, front) = ready_front(&[&flag]).await;
    let (code, body) = get(front.api, "/readyz/reapi").await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(failing(&body), ["no_executor"], "{body}");
}
