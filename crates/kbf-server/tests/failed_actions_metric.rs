//! `kbf_execute_failed_actions_total{class}` on `GET /metrics` of the operator API
//! (`docs/api.md#get-metrics`), after Executes that fail and that do not.

mod support;

use std::net::SocketAddr;

use support::{Cell, Job, done, output, ran, response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The value of the series of `class` in a scrape of `api`.
async fn count(api: SocketAddr, class: &str) -> u64 {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut text = String::new();
    tcp.read_to_string(&mut text).await.expect("read");
    let prefix = format!("kbf_execute_failed_actions_total{{class=\"{class}\"}} ");
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no series {prefix}in {text}"));
    line.parse().expect("a count")
}

/// Asserts that a server shows every class at 0 before any Execute, counts an action that
/// exits non-zero as one `action` failure and nothing else, and counts neither an action
/// that exits 0 nor a cache hit of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_action_raises_only_the_action_class() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    for class in ["farm", "request", "action", "ambiguous"] {
        assert_eq!(count(api, class).await, 0, "{class}");
    }
    let mut daemon = cell.daemon("node-a", 4, 8).await;

    let passes = Job::new("passes", &[]);
    cell.upload(&passes.blobs()).await;
    let mut ops = cell.execute(&passes.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "built", 0).await;
    let mut report = ran(start.lease_id, &result);
    report.status = None;
    assert!(daemon.report(report).await.accepted);
    let _ = response(&done(&mut ops).await);
    assert_eq!(count(api, "action").await, 0);

    let fails = Job::new("fails", &[]);
    cell.upload(&fails.blobs()).await;
    let mut ops = cell.execute(&fails.action).await;
    let start = daemon.start().await;
    let result = output(&cell, "a test failed", 1).await;
    assert!(daemon.report(ran(start.lease_id, &result)).await.accepted);
    let _ = response(&done(&mut ops).await);
    assert_eq!(count(api, "action").await, 1);
    for class in ["farm", "request", "ambiguous"] {
        assert_eq!(count(api, class).await, 0, "{class}");
    }
}
