//! The pack-read counts a daemon sends on its heartbeats, on `GET /metrics` of the operator API
//! of a server (`docs/api.md#get-metrics`): `kbf_pack_daemon_*`, a counter per count and a
//! sample per node.

mod support;

use std::net::SocketAddr;

use kbf_proto::worker::{PackReadCounts, PackWriteCounts};
use support::Cell;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The text `GET /metrics` at `api` answers.
async fn scrape(api: SocketAddr) -> String {
    let mut tcp = TcpStream::connect(api).await.expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    tcp.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("read");
    response
}

/// Asserts that a server shows the counts of the newest heartbeat of each node that sent any, as
/// `kbf_pack_daemon_*` counters labelled with the node: none of the families before a daemon has
/// sent counts, a node that sent none has no sample, a later heartbeat replaces the counts, and
/// a heartbeat without counts removes the node's samples.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_shows_the_pack_reads_its_daemons_report() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_daemon_"), "{text}");

    let mut reading = cell.daemon("node-a", 8, 16).await;
    let mut plain = cell.daemon("node-b", 8, 16).await;
    let counts = PackReadCounts {
        served: 5,
        served_bytes: 4096,
        elsewhere: 2,
        unasked: 1,
        lost: 3,
        damaged: 4,
    };
    reading.beat_acked(&[], Some(counts)).await;
    plain.beat_acked(&[], None).await;
    let text = scrape(api).await;
    for (family, want) in [
        ("served", 5),
        ("served_bytes", 4096),
        ("elsewhere", 2),
        ("unasked", 1),
        ("lost", 3),
        ("damaged", 4),
    ] {
        let header = format!("# TYPE kbf_pack_daemon_{family}_total counter\n");
        assert!(text.contains(&header), "{header}{text}");
        let sample = format!("kbf_pack_daemon_{family}_total{{node=\"node-a\"}} {want}\n");
        assert!(text.contains(&sample), "{sample}{text}");
    }
    assert!(!text.contains("node=\"node-b\""), "{text}");

    let newer = PackReadCounts {
        served: 6,
        ..counts
    };
    reading.beat_acked(&[], Some(newer)).await;
    let text = scrape(api).await;
    let sample = "kbf_pack_daemon_served_total{node=\"node-a\"} 6\n";
    assert!(text.contains(sample), "{text}");

    reading.beat_acked(&[], None).await;
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_daemon_"), "{text}");
}

/// Write counts in which every count is told apart by its value, 1 to 12 in field order.
fn written() -> PackWriteCounts {
    PackWriteCounts {
        packs: 1,
        packed: 2,
        packed_bytes: 3,
        fallback_off: 4,
        fallback_unfit: 5,
        fallback_unsupported: 6,
        fallback_refused: 7,
        fallback_store: 8,
        fallback_record: 9,
        fallback_index: 10,
        missing_indexed: 11,
        missing_unasked: 12,
    }
}

/// Asserts that a server shows the pack-write counts of the newest heartbeat of each node that
/// sent any, as `kbf_pack_daemon_*` counters: a node that sent only read counts has none of the
/// write samples, the fallbacks have one sample per reason, a later heartbeat replaces the
/// counts, and a heartbeat without write counts removes the node's write samples and leaves its
/// read samples.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_shows_the_pack_writes_its_daemons_report() {
    let cell = Cell::start_with_api().await;
    let api = cell.api.expect("an operator API");
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_daemon_"), "{text}");

    let mut writing = cell.daemon("node-a", 8, 16).await;
    let mut reading = cell.daemon("node-b", 8, 16).await;
    let reads = PackReadCounts {
        served: 7,
        ..PackReadCounts::default()
    };
    writing
        .beat_counted_acked(Some(reads), Some(written()))
        .await;
    reading.beat_counted_acked(Some(reads), None).await;
    let text = scrape(api).await;
    for (family, want) in [
        ("packs_written", 1),
        ("blobs_packed", 2),
        ("packed_bytes", 3),
        ("missing_indexed", 11),
        ("missing_unasked", 12),
    ] {
        let header = format!("# TYPE kbf_pack_daemon_{family}_total counter\n");
        assert!(text.contains(&header), "{header}{text}");
        let sample = format!("kbf_pack_daemon_{family}_total{{node=\"node-a\"}} {want}\n");
        assert!(text.contains(&sample), "{sample}{text}");
    }
    let reasons = [
        "off",
        "unfit",
        "unsupported",
        "refused",
        "store",
        "record",
        "index",
    ];
    for (i, reason) in reasons.iter().enumerate() {
        let family = "kbf_pack_daemon_write_fallbacks_total";
        let sample = format!(
            "{family}{{node=\"node-a\",reason=\"{reason}\"}} {}\n",
            i + 4
        );
        assert!(text.contains(&sample), "{sample}{text}");
    }
    let node_b = text.lines().filter(|l| l.contains("node=\"node-b\""));
    let node_b: Vec<&str> = node_b.collect();
    assert_eq!(node_b.len(), 6, "{text}");
    assert!(
        node_b.iter().all(|l| l.starts_with("kbf_pack_daemon_")),
        "{text}"
    );
    let write_samples = text
        .lines()
        .filter(|l| l.contains("_fallbacks_total{node=\"node-b\""));
    assert_eq!(write_samples.count(), 0, "{text}");

    let newer = PackWriteCounts {
        packs: 9,
        ..written()
    };
    writing.beat_counted_acked(Some(reads), Some(newer)).await;
    let text = scrape(api).await;
    let sample = "kbf_pack_daemon_packs_written_total{node=\"node-a\"} 9\n";
    assert!(text.contains(sample), "{text}");

    writing.beat_counted_acked(Some(reads), None).await;
    let text = scrape(api).await;
    assert!(
        !text.contains("kbf_pack_daemon_packs_written_total"),
        "{text}"
    );
    assert!(
        !text.contains("kbf_pack_daemon_write_fallbacks_total"),
        "{text}"
    );
    let served = "kbf_pack_daemon_served_total{node=\"node-a\"} 7\n";
    assert!(text.contains(served), "{text}");
}
