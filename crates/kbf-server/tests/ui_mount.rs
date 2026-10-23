//! The web UI mounted on the operator API of a real in-process server (`/ui`,
//! `kbf_server::ui_mount`): open, read-only, with the security headers on every answer,
//! and bounded per caller. Each test names the defect it catches.

mod pages;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use kbf_invocation::Outcome;
use kbf_ui::CONTENT_SECURITY_POLICY;
use pages::{Answer, Server, at, digest, finished, granted, queued_target, started, text};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

/// Every REAPI call refused (`deny`), and every read allowed: the policy of a farm that
/// admits nobody to the build pages by the network route (`GET /v1/invocations/{id}` is
/// `401`) while the pages of the UI are open.
const NO_CALLERS: &str = r#"{"authenticationPolicy": {"deny": "no callers"},
    "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#;

/// A policy file with no read keys at all: the build pages, blobs and host updates are off
/// on the network route.
const NO_READ_KEYS: &str = r#"{"authenticationPolicy": {"allow": {}}}"#;

/// A server with the build `b1`: a failed action whose stderr holds markup.
async fn with_a_failed_build(policy: &str) -> (Server, String) {
    let server = Server::with(policy);
    let action = digest(7);
    let stderr = server.store(b"<script>alert(1)</script>\n").await;
    let result = pages::listing(digest(8), 1, &[], (None, Some(stderr)));
    server
        .feed([
            queued_target("b1", "//app:lib", at(0), action, 1),
            granted(1, 1, "n4", at(1)),
            started(1, 1, at(2)),
            finished(
                1,
                1,
                Outcome::Failed {
                    code: 2,
                    message: "boom".to_owned(),
                },
                Some(result),
                at(3),
            ),
        ])
        .await;
    let action_page = format!("/ui/builds/b1/actions/{}", text(&action));
    (server, action_page)
}

fn assert_secure(answer: &Answer, what: &str) {
    assert_eq!(
        answer.header("content-security-policy"),
        Some(CONTENT_SECURITY_POLICY),
        "{what}: {}",
        answer.head
    );
    assert_eq!(
        answer.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(
        answer.header("referrer-policy"),
        Some("no-referrer"),
        "{what}"
    );
    assert_eq!(answer.header("cache-control"), Some("no-store"), "{what}");
    assert_eq!(
        answer.header("cross-origin-resource-policy"),
        Some("same-origin"),
        "{what}"
    );
    assert_eq!(
        answer.header("cross-origin-opener-policy"),
        Some("same-origin"),
        "{what}"
    );
    let policy = answer.header("permissions-policy").expect("a policy");
    for feature in ["camera", "microphone", "geolocation", "usb", "payment"] {
        assert!(
            policy.contains(&format!("{feature}=()")),
            "{what}: {feature} is not denied in {policy}"
        );
    }
    assert_eq!(answer.header("strict-transport-security"), None, "{what}");
}

/// Catches (the ruling that the UI is open): a page that asks the policy-gated reads
/// as the network does, so the policy's authenticator refuses it (mutant: the UI's
/// routes built over the server's policy instead of the open one: every page is a `502`
/// or `404`); the policy's gates removed from the network route instead (the same
/// request to `/v1/invocations` must still be `401` or `404`); a log read through the
/// blob route that is not shown as escaped text.
#[tokio::test]
async fn the_ui_is_open_whatever_the_policy_says_of_the_network_routes() {
    for policy in [NO_CALLERS, NO_READ_KEYS] {
        let (server, action_page) = with_a_failed_build(policy).await;
        // The network route keeps its gate.
        let network = server.get("/v1/invocations/b1").await;
        assert!(
            matches!(network.code, 401 | 404),
            "{policy}: {} {}",
            network.code,
            network.head
        );

        // The UI is not gated by it.
        let overview = server.get("/ui").await;
        assert_eq!(overview.code, 200, "{}", overview.text());
        let lookup = server.get("/ui/builds?id=b1").await;
        assert_eq!(lookup.code, 200, "{}", lookup.text());
        assert!(
            lookup.text().contains("href=\"/ui/builds/b1\""),
            "{}",
            lookup.text()
        );
        let build = server.get("/ui/builds/b1").await;
        assert_eq!(build.code, 200, "{}", build.text());
        assert!(build.text().contains("//app:lib"), "{}", build.text());
        let action = server.get(&action_page).await;
        assert_eq!(action.code, 200, "{}", action.text());
        // The stderr came through the blob route, open, and is text.
        assert!(
            action
                .text()
                .contains("<pre>&lt;script&gt;alert(1)&lt;/script&gt;\n</pre>"),
            "{}",
            action.text()
        );
        assert!(!action.text().contains("<script"), "{}", action.text());
        let css = server.get("/ui/static/kbf.css").await;
        assert_eq!(css.code, 200);
        assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));
        server.stop().await;
    }
}

/// Catches: a UI that is on when it was turned off (mutant: `ui` ignored), and one whose
/// absence takes more with it than `/ui`.
#[tokio::test]
async fn ui_false_leaves_no_ui_and_every_other_route() {
    let policy = kbf_auth::Policy::from_json(pages::OPEN).expect("a policy");
    let server = Server::start_ui(policy, kbf_front::DEFAULT_INVOCATION_MEMORY, None, false);
    for path in ["/ui", "/ui/", "/ui/builds", "/ui/static/kbf.css"] {
        let answer = server.get(path).await;
        assert_eq!(answer.code, 404, "{path}: {}", answer.head);
        assert!(answer.header("content-security-policy").is_none(), "{path}");
    }
    assert_eq!(server.get("/v1/nodes").await.code, 200);
    assert_eq!(server.get("/readyz").await.code, 200);
    assert_eq!(server.get("/v1/invocations/none").await.code, 404);
    server.stop().await;
}

/// Catches: the removed Updates page still answering: `/ui/updates` and a path below it
/// must answer exactly as a UI path that never existed, and the Overview must not link it.
#[tokio::test]
async fn the_updates_page_is_gone_and_answers_like_any_unknown_ui_path() {
    let (server, _) = with_a_failed_build(pages::OPEN).await;
    let unknown = server.get("/ui/never-a-page").await;
    assert_eq!(unknown.code, 404, "{}", unknown.head);
    for path in ["/ui/updates", "/ui/updates/plan"] {
        let answer = server.get(path).await;
        assert_eq!(answer.code, unknown.code, "{path}: {}", answer.head);
        // The not-found page names the path it was asked for; the rest is the same.
        assert_eq!(
            answer.text().replace(path, "PATH"),
            unknown.text().replace("/ui/never-a-page", "PATH"),
            "{path}"
        );
    }
    let overview = server.get("/ui").await;
    assert_eq!(overview.code, 200);
    assert!(
        !overview.text().contains("/ui/updates"),
        "{}",
        overview.text()
    );
    server.stop().await;
}

/// Catches: a mount that serves a method other than `GET` and `HEAD` (mutant: `any`
/// for `get`), `HEAD` with a body or refused, and any answer, a `405` or `404` included,
/// without the headers (mutant: the `secure` layer removed).
#[tokio::test]
async fn the_ui_answers_get_and_head_only_and_every_answer_is_secured() {
    let (server, action_page) = with_a_failed_build(pages::OPEN).await;
    for path in [
        "/ui",
        "/ui/",
        "/ui/builds",
        "/ui/builds?id=b1",
        "/ui/builds/b1",
        action_page.as_str(),
        "/ui/static/kbf.css",
        "/ui/missing",
        "/ui/builds/none",
    ] {
        let answer = server.get(path).await;
        assert_secure(&answer, path);
        assert!(!answer.header("content-type").unwrap_or_default().is_empty());
    }
    let head = server.request("HEAD", "/ui", "").await;
    assert_eq!(head.code, 200);
    assert!(head.body.is_empty());
    assert_secure(&head, "HEAD");
    for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        for path in ["/ui", "/ui/builds/b1", "/ui/static/kbf.css"] {
            let answer = server.request(method, path, "").await;
            assert_eq!(answer.code, 405, "{method} {path}: {}", answer.head);
            assert_secure(&answer, &format!("{method} {path}"));
        }
    }
    // No script, nothing cross-origin: the headers say so, the pages carry no script, and
    // no CORS header is ever sent.
    let page = server.get("/ui/builds/b1").await;
    assert!(!page.text().contains("<script"), "{}", page.text());
    assert!(page.header("access-control-allow-origin").is_none());
    server.stop().await;
}

/// Catches: a path parameter nothing bounds (mutant: the validation removed from the
/// limiter): a control character or a very long segment reaching a page, a route, a log
/// line or an upstream URL; on the pages and on the read routes they use alike.
#[tokio::test]
async fn hostile_path_parameters_are_refused_on_the_pages_and_the_read_routes() {
    let (server, _) = with_a_failed_build(pages::OPEN).await;
    let long = "a".repeat(300);
    for (path, want) in [
        ("/ui/builds/%00", 400),
        ("/ui/builds/a%0Ab", 400),
        (&format!("/ui/builds/{long}"), 400),
        ("/v1/blobs/%00", 400),
        (&format!("/v1/invocations/{long}"), 400),
        (&format!("/v1/runs/{long}"), 400),
        ("/v1/blobs/%1f", 400),
    ] {
        let answer = server.get(path).await;
        assert_eq!(answer.code, want, "{path}: {}", answer.head);
    }
    let too_long = format!("/ui/{}", "a/".repeat(2000));
    assert_eq!(server.get(&too_long).await.code, 414);
    assert_eq!(
        server
            .get(&format!("/v1/blobs/{}", "a/".repeat(2000)))
            .await
            .code,
        414
    );
    let query = format!("/ui/builds?id={}", "q".repeat(5000));
    assert_eq!(server.get(&query).await.code, 414);
    // A plain unknown id is the route's own `404`.
    assert_eq!(server.get("/ui/builds/none").await.code, 404);
    server.stop().await;
}

/// Opens a connection from `from` to `api`, sends a `GET` of `path` and reads only the
/// head of the answer: the body is left unread, so the server holds the read open.
async fn open_read(api: SocketAddr, from: Ipv4Addr, path: &str) -> (TcpStream, String) {
    let socket = TcpSocket::new_v4().expect("a socket");
    socket
        .bind(SocketAddr::from((from, 0)))
        .expect("bind a loopback address");
    let mut stream = socket.connect(api).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut head = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.expect("read the head");
        assert!(
            n > 0,
            "closed before a head: {}",
            String::from_utf8_lossy(&head)
        );
        head.extend_from_slice(&chunk[..n]);
    }
    (stream, String::from_utf8_lossy(&head).into_owned())
}

/// A `GET` of `path` from `from`, read to its end.
async fn get_from(api: SocketAddr, from: Ipv4Addr, path: &str) -> u16 {
    let socket = TcpSocket::new_v4().expect("a socket");
    socket
        .bind(SocketAddr::from((from, 0)))
        .expect("bind a loopback address");
    let mut stream = socket.connect(api).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    String::from_utf8_lossy(&response)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code")
}

/// Catches (the starvation test of R3): a caller that opens many slow reads taking every
/// read of the server (mutant: the limiter removed from the read routes: the fifth read
/// of the same caller is served); one caller's reads counted against another's (mutant:
/// one key for all callers); `/ui` and the API sharing one bound, so that a page cannot be
/// opened while the API is saturated (mutant: one `ReadLimit` for both); a slot that never
/// comes back when the reader goes away (mutant: the guard leaked); a `429` without
/// `Retry-After`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_with_slow_reads_open_cannot_starve_another_or_the_ui() {
    let (server, _) = with_a_failed_build(pages::OPEN).await;
    // Larger than any socket buffer between two loopback ends, so a reader that does not
    // read keeps the server's write, and its slot, open.
    let big = server.store(&vec![7_u8; 48 * 1024 * 1024]).await;
    let small = server.store(b"small").await;
    let (big, small) = (
        format!("/v1/blobs/{}", text(&big)),
        format!("/v1/blobs/{}", text(&small)),
    );
    let api = server.api;
    // Another loopback address: a second caller, to the server that sees the peer address.
    let other = Ipv4Addr::new(127, 0, 0, 2);

    let mut slow = Vec::new();
    for _ in 0..kbf_server::read_limit::PER_CALLER {
        let (stream, head) = open_read(api, Ipv4Addr::LOCALHOST, &big).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        slow.push(stream);
    }
    // The same caller is refused, on every read route, with a wait.
    let refused = server.get(&small).await;
    assert_eq!(refused.code, 429, "{}", refused.head);
    assert_eq!(refused.header("retry-after"), Some("1"));
    assert_eq!(server.get("/v1/invocations/b1").await.code, 429);

    // Another caller reads, and so does a page of the same caller (its own bound).
    assert_eq!(get_from(api, other, &small).await, 200);
    assert_eq!(get_from(api, other, "/v1/invocations/b1").await, 200);
    let page = server.get("/ui/builds?id=b1").await;
    assert_eq!(page.code, 200, "{}", page.text());

    // The readers go away: the slots come back.
    drop(slow);
    let mut served = false;
    for _ in 0..200 {
        if server.get(&small).await.code == 200 {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(served, "the slots never came back");
    server.stop().await;
}
