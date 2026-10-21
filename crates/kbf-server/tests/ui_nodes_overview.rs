//! The Overview, Nodes and Node pages of `kbf-ui` over a real server and real daemons
//! (`docs/design/kbf-ui-v1.md`, P3 and P7): the pages are mounted on the operator API and
//! read its `/v1` routes in process, so a `200` page is proof that the page parses what the
//! routes answer (a key the page reads that a route does not send is a `502`), and the
//! values on the page are the ones the same routes answer over TCP. Each test names the
//! defect it catches.

#[path = "support/api_http.rs"]
mod api_http;
mod support;

use std::net::SocketAddr;

use api_http::get;
use kbf_ui::CONTENT_SECURITY_POLICY;
use support::{Cell, Job};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `GET path` of the UI with no credential: the status, the head and the body as text.
async fn page(api: SocketAddr, path: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.expect("read");
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    (code, head.to_owned(), body.to_owned())
}

/// Catches: a page that cannot read the real documents (a `502`: a key renamed, a type
/// changed), a value on a page that is not the route's (a lease count, a booking, a
/// capacity, a count of nodes by state, the queue depth or the lease sum; every one is
/// compared with the JSON the same server answers over TCP), a node id written as markup
/// (the id of one daemon is HTML), and a page without the security headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pages_show_what_the_routes_answer() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let mut busy = cell.daemon("linux-1", 8, 16).await;
    // Sorts after linux-1, so the lease goes to linux-1.
    let _hostile = cell.daemon("z<i>odd</i>&\"'", 4, 8).await;
    let job = Job::new("compile", &[]);
    cell.upload(&job.blobs()).await;
    let _operation = cell.execute(&job.action).await;
    let lease = busy.start().await.lease_id.expect("a lease");
    let lease_name = format!("{}.{}", lease.term, lease.seq);

    let (_, nodes) = get(api, "/v1/nodes").await;
    let linux = nodes["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["node_id"] == "linux-1")
        .expect("listed");
    let booked_cpu = linux["booked"]["cpu_millis"].as_u64().expect("cpu");
    assert!(booked_cpu > 0, "the lease books something: {linux}");

    let (code, head, list) = page(api, "/ui/nodes").await;
    assert_eq!(code, 200, "{list}");
    assert!(
        head.to_lowercase().contains(&format!(
            "content-security-policy: {CONTENT_SECURITY_POLICY}"
        )),
        "{head}"
    );
    let row = list
        .split("<tr>")
        .find(|r| r.contains("href=\"/ui/nodes/linux-1\""))
        .expect("the row of linux-1");
    assert!(row.contains("<td>1</td>"), "one lease: {row}");
    assert!(row.contains(&format!("{booked_cpu} mCPU")), "{row}");
    assert!(row.contains("pill ok\">connected<"), "{row}");
    assert!(!list.contains("<i>odd</i>"), "the id is text: {list}");
    assert!(
        list.contains("z&lt;i&gt;odd&lt;/i&gt;&amp;&quot;&#39;"),
        "{list}"
    );

    let (code, _, detail) = page(api, "/ui/nodes/linux-1").await;
    assert_eq!(code, 200, "{detail}");
    let (_, json) = get(api, "/v1/nodes/linux-1").await;
    let free_cpu = json["free"]["cpu_millis"].as_u64().expect("free cpu");
    for text in [
        format!("<li><code>{lease_name}</code></li>"),
        format!("<th scope=\"row\">Capacity</th><td>{}</td>", 8000),
        format!("<th scope=\"row\">Booked</th><td>{booked_cpu}</td>"),
        format!("<th scope=\"row\">Free</th><td>{free_cpu}</td>"),
    ] {
        assert!(detail.contains(&text), "{text}: {detail}");
    }
    let (code, _, odd) = page(api, "/ui/nodes/z%3Ci%3Eodd%3C%2Fi%3E%26%22%27").await;
    assert_eq!(code, 200, "{odd}");
    assert!(!odd.contains("<i>odd</i>"), "{odd}");
    let (code, _, ghost) = page(api, "/ui/nodes/ghost").await;
    assert_eq!(code, 404, "{ghost}");
    let (code, _, verb) = page(api, "/ui/nodes/linux-1%3Acordon").await;
    assert_eq!(code, 404, "a write's path is no node: {verb}");

    let (_, health) = get(api, "/v1/health").await;
    let (code, _, overview) = page(api, "/ui").await;
    assert_eq!(code, 200, "{overview}");
    for (label, key) in [
        ("nodes", "total"),
        ("serving", "serving"),
        ("disconnected", "disconnected"),
    ] {
        let n = health["nodes"][key].as_u64().expect("a count");
        let cell = format!("<span class=\"n\">{n}</span><span class=\"label\">{label}</span>");
        assert!(overview.contains(&cell), "{label}: {overview}");
    }
    let depth = health["queue"]["depth"].as_u64().expect("depth");
    let leases = health["leases"].as_u64().expect("leases");
    assert_eq!(leases, 1, "{health}");
    assert!(
        overview.contains(&format!("<dt>Queue depth</dt><dd>{depth}</dd>")),
        "{overview}"
    );
    assert!(
        overview.contains(&format!("<dt>Leases held</dt><dd>{leases}</dd>")),
        "{overview}"
    );
    assert!(
        overview.contains("<a href=\"/v1/health\">/v1/health</a>"),
        "the page lists what it read: {overview}"
    );
}

/// Catches: the refresh reaching the wrong place or being ignored through the mount
/// (`?refresh=` is the page's own query), and a bad value answered `200`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_refresh_is_opt_in_and_validated_through_the_mount() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an API address");
    let _node = cell.daemon("linux-1", 8, 16).await;
    for path in ["/ui", "/ui/nodes", "/ui/nodes/linux-1"] {
        let (code, _, plain) = page(api, path).await;
        assert_eq!(code, 200, "{path}");
        assert!(!plain.contains("http-equiv"), "{path}");
        let (code, _, fast) = page(api, &format!("{path}?refresh=10")).await;
        assert_eq!(code, 200, "{path}");
        assert!(
            fast.contains("<meta http-equiv=\"refresh\" content=\"10\">"),
            "{path}: {fast}"
        );
        let (code, _, bad) = page(api, &format!("{path}?refresh=3")).await;
        assert_eq!(code, 400, "{path}: {bad}");
    }
}
