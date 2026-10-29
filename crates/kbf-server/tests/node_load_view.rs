//! The node load a daemon sends on its heartbeats, in `GET /v1/nodes/{id}` of the operator API
//! (`docs/api.md#node-load`): left out before any reading, shown from the readings the server
//! accepts, and unchanged by a heartbeat that carries none.

mod support;

use std::net::SocketAddr;

use kbf_proto::worker::{LeaseId, NodeLoad};
use support::Cell;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The JSON body `GET /v1/nodes/{node}` at `api` answers.
async fn node(api: SocketAddr, node: &str) -> serde_json::Value {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request =
        format!("GET /v1/nodes/{node} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a response");
    assert!(head.starts_with("HTTP/1.1 200"), "{response}");
    // The body may be chunked: the JSON is the text from the first brace to the last.
    let (start, end) = (
        body.find('{').expect("json"),
        body.rfind('}').expect("json"),
    );
    serde_json::from_str(&body[start..=end]).expect("json")
}

fn reading(seq: u64, mono_usec: u64, cpu_usage_usec: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec,
        cpu_usage_usec,
        memory_working_set_bytes: 1_000,
        memory_active_file_bytes: 200,
        covered: vec![LeaseId { term: 3, seq: 4 }],
        cpu_throttled_usec: Some(7),
        ..NodeLoad::default()
    }
}

/// Asserts that a node view has no `load` before a reading, shows the accepted readings of the
/// heartbeats after, keeps showing them across a heartbeat without one, and drops a reading whose
/// seq is not above the last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "known-different-on-js: the js core keeps no node load"
)]
async fn a_server_shows_the_node_load_its_daemons_report() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    let mut daemon = cell.daemon("node-a", 8, 16).await;

    daemon.beat_loaded_acked(None).await;
    assert!(node(api, "node-a").await.get("load").is_none());

    daemon
        .beat_loaded_acked(Some(reading(1, 1_000_000, 0)))
        .await;
    daemon
        .beat_loaded_acked(Some(reading(2, 2_000_000, 2_500_000)))
        .await;
    let view = node(api, "node-a").await;
    let load = &view["load"];
    assert_eq!(load["measured_cpu_millicores"], 2_500);
    assert_eq!(load["memory_working_set_bytes"], 1_000);
    assert_eq!(load["memory_active_file_bytes"], 200);
    assert_eq!(load["admission_working_set_bytes"], 1_000);
    assert_eq!(load["covered"], serde_json::json!(["3.4"]));
    assert_eq!(load["actions_cpu_limited"], false);
    assert_eq!(load["cpu_throttled_usec"], 7);
    assert!(load.get("cpu_pressure_some_total_usec").is_none());
    assert_eq!(load["stale"], false);

    daemon.beat_loaded_acked(None).await;
    daemon
        .beat_loaded_acked(Some(reading(2, 9_000_000, 9_000_000)))
        .await;
    assert_eq!(node(api, "node-a").await["load"], *load);
}
