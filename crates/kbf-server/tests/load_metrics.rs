//! The scheduler's measured-load counters and each node's load on `GET /metrics` of the operator
//! API of a server (`docs/api.md#get-metrics`): `kbf_sched_load_*` and `kbf_node_load_*`, read
//! off a real scrape over the heartbeats of daemons on the worker stream.

mod support;

use std::net::SocketAddr;

use kbf_proto::worker::NodeLoad;
use support::Cell;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SECOND: u64 = 1_000_000;
/// An hour, in seconds.
const HOUR: u64 = 3600;

/// The text `GET /metrics` at `api` answers.
async fn scrape(api: SocketAddr) -> String {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    response
}

/// A reading `seq` taken at `mono` microseconds, with `usage` microseconds of CPU used in all.
fn reading(seq: u64, mono: u64, usage: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: mono,
        cpu_usage_usec: usage,
        memory_working_set_bytes: 4096,
        ..NodeLoad::default()
    }
}

/// Asserts that a server's scrape has the scheduler's counters at 0 and no node family before a
/// daemon reports a load; that after one reading a node has its gauges but no measured CPU (no
/// window yet), and after a second one the measured CPU and the scheduler's accepted count; and
/// that a node that reported no load has no `kbf_node_load_` sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_shows_the_load_its_daemons_report() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    let text = scrape(api).await;
    assert!(
        text.contains("kbf_sched_load_readings_accepted_total 0\n"),
        "{text}"
    );
    assert!(!text.contains("kbf_node_load_"), "{text}");

    let mut loaded = cell.daemon("node-a", 8, 16).await;
    let mut plain = cell.daemon("node-b", 8, 16).await;
    plain.beat_loaded_acked(None).await;
    loaded
        .beat_loaded_acked(Some(reading(1, 10 * SECOND, 0)))
        .await;
    let text = scrape(api).await;
    for sample in [
        "kbf_sched_load_readings_accepted_total 1\n",
        "kbf_node_load_memory_working_set_bytes{node=\"node-a\"} 4096\n",
        "kbf_node_load_stale{node=\"node-a\"} 0\n",
    ] {
        assert!(text.contains(sample), "{sample}{text}");
    }
    assert!(
        !text.contains("kbf_node_load_measured_cpu_millicores"),
        "{text}"
    );

    // This server reads wall time, so the node must not go stale however long the runner stalls
    // between the heartbeats: it is stale after three windows, and the window between these two
    // readings is an hour (3600 s of the daemon's clock, using half of it in CPU), so a stall of
    // three hours would be needed.
    loaded
        .beat_loaded_acked(Some(reading(2, (10 + HOUR) * SECOND, HOUR * SECOND / 2)))
        .await;
    let text = scrape(api).await;
    for sample in [
        "kbf_sched_load_readings_accepted_total 2\n",
        "kbf_node_load_measured_cpu_millicores{node=\"node-a\"} 500\n",
        "kbf_node_load_stale{node=\"node-a\"} 0\n",
    ] {
        assert!(text.contains(sample), "{sample}{text}");
    }
    assert!(!text.contains("node=\"node-b\""), "{text}");
}
