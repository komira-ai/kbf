//! The node load a daemon sends on its heartbeats, in `GET /v1/nodes/{id}` of the operator API
//! (`docs/api.md#node-load`): left out before any reading, shown from the readings the server
//! accepts, and unchanged by a heartbeat that carries none.

mod support;

use std::net::SocketAddr;

use kbf_proto::worker::{LeaseClock, LeaseId, LeaseMinute, NodeLoad};
use support::{Cell, Job};
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

fn lease(seq: u64) -> LeaseId {
    LeaseId { term: 3, seq }
}

fn minute(seq: u64, minute: u32) -> LeaseMinute {
    LeaseMinute {
        lease: Some(lease(seq)),
        minute,
        cpu_mean_millis: 100,
        cpu_peak_window_millis: 200,
        ws_max_bytes: 4_096,
    }
}

/// Asserts that the node view counts the records the lists of a node's readings carried, each
/// drop under its reason, and the minutes of a reading the server refused; that a heartbeat
/// with no reading changes none of it; and that the scheduler, on the same heartbeats, is
/// handed the accepted readings and none of the refused one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_node_view_shows_the_list_counts_and_the_scheduler_hears_accepted_readings() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    let mut daemon = cell.daemon("node-a", 8, 16).await;
    let counts = |clocks: u64, minutes: u64, invalid: u64, refused: u64| {
        serde_json::json!({
            "clocks": clocks,
            "minutes": minutes,
            "dropped": { "invalid": invalid, "duplicate": 0, "over_cap": 0 },
            "refused_minutes": refused,
        })
    };

    let first = NodeLoad {
        lease_clocks: vec![LeaseClock {
            lease: Some(lease(4)),
            started_mono_usec: 500_000,
        }],
        minutes: vec![minute(4, 0)],
        ..reading(1, 1_000_000, 0)
    };
    daemon.beat_loaded_acked(Some(first)).await;
    assert_eq!(
        node(api, "node-a").await["load"]["lists"],
        counts(1, 1, 0, 0)
    );
    assert_eq!(cell.load_counters().readings_accepted, 1);

    // A minute whose mean is above its peak is dropped, alone: the other is kept.
    let broken = LeaseMinute {
        cpu_mean_millis: 300,
        ..minute(4, 2)
    };
    let second = NodeLoad {
        minutes: vec![minute(4, 1), broken],
        ..reading(2, 2_000_000, 2_500_000)
    };
    daemon.beat_loaded_acked(Some(second)).await;
    let view = node(api, "node-a").await;
    assert_eq!(view["load"]["lists"], counts(1, 2, 1, 0));
    assert_eq!(cell.load_counters().readings_accepted, 2);

    // No reading: nothing changes.
    daemon.beat_loaded_acked(None).await;
    assert_eq!(
        node(api, "node-a").await["load"]["lists"],
        counts(1, 2, 1, 0)
    );
    assert_eq!(cell.load_counters().readings_accepted, 2);

    // A repeat of the last seq is refused: its two minutes are counted, and the scheduler
    // does not hear it.
    let repeat = NodeLoad {
        minutes: vec![minute(4, 3), minute(4, 4)],
        ..reading(2, 3_000_000, 3_000_000)
    };
    daemon.beat_loaded_acked(Some(repeat)).await;
    assert_eq!(
        node(api, "node-a").await["load"]["lists"],
        counts(1, 2, 1, 2)
    );
    assert_eq!(cell.load_counters().readings_accepted, 2);
    assert_eq!(cell.load_counters().readings_dropped, 0);
}

/// Asserts that a load far past every cap, with an absent lease and a repeated one in it,
/// costs its heartbeat nothing: it is acknowledged and the running list it carries is applied
/// (a lease the server gave up, which the heartbeat lists, is cancelled; the lease it holds is
/// not); that the view shows the records kept within the caps and every drop under its
/// reason; and that the scheduler took the reading.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_load_does_not_cost_the_beat_and_its_drops_are_counted() {
    const N: u64 = 100_000;
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    let mut first = cell.daemon("node-a", 8, 16).await;
    let job = Job::new("hostile", &[]);
    cell.upload(&job.blobs()).await;
    let _ops = cell.execute(&job.action).await;
    let lost = first.start().await.lease_id.expect("a lease id");

    // The daemon returns and its first heartbeat leaves the lease out: the server gives it up
    // and places the operation again, so `lost` is one it no longer holds and `retry` one it
    // does.
    let mut daemon = cell.daemon("node-a", 8, 16).await;
    assert!(daemon.heartbeat(&[]).await);
    let retry = daemon.start().await.lease_id.expect("a lease id");
    assert_ne!(retry, lost);

    let clock = |i: u64| LeaseClock {
        // The first names no lease, the second is the lease of the third.
        lease: match i {
            0 => None,
            1 => Some(lease(2)),
            _ => Some(lease(i)),
        },
        started_mono_usec: 0,
    };
    let hostile = NodeLoad {
        covered: (0..N).map(lease).collect(),
        lease_clocks: (0..N).map(clock).collect(),
        minutes: (0..N)
            .map(|i| minute(i / 64, u32::try_from(i % 64).expect("below 64")))
            .collect(),
        ..reading(1, 1_000_000, 0)
    };
    daemon
        .beat_running_loaded_acked(&[lost, retry], Some(hostile))
        .await;
    // Every cancel the heartbeat caused arrived before its acknowledgement: exactly one, for
    // the lost lease, so the held lease was not cancelled.
    assert_eq!(
        daemon.cancels_received(),
        vec![lost],
        "the running list of the hostile heartbeat was not applied"
    );

    let view = node(api, "node-a").await;
    let load = &view["load"];
    assert_eq!(load["covered"].as_array().map(Vec::len), Some(96));
    // Kept: 64 minutes; 94 clocks (96 are looked at: one absent, one repeated). Cut off:
    // 99,936 minutes and 99,904 each of clocks and `covered`.
    assert_eq!(
        load["lists"],
        serde_json::json!({
            "clocks": 94,
            "minutes": 64,
            "dropped": { "invalid": 1, "duplicate": 1, "over_cap": 299_744 },
            "refused_minutes": 0,
        })
    );
    assert_eq!(cell.load_counters().readings_accepted, 1);
}
