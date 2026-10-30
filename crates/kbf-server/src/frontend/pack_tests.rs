//! A frontend started with the pack flags, over an S3 service on loopback and an in-memory pack
//! index: its CAS calls keep blobs in packs, its metrics count them, and the checks of its start
//! refuse what cannot work. Without the flags it keeps nothing in packs and contacts no store.

use std::error::Error;
use std::future::pending;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_objstore::s3::fake::FakeS3;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, PackIndex};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchUpdateBlobsRequest, FindMissingBlobsRequest,
    batch_update_blobs_request,
};
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, InputCacheCounts, PackReadCounts, PackWriteCounts,
    ServerMessage, TreeCounts, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_state::ManualClock;
use kbf_types::Digest;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

use super::{FrontendFlags, FrontendHandle, Prepared, boxed, command, parse};
use crate::test_dir::scratch_dir;

/// The flags of a frontend on loopback ports, with a Postgres state store that is only parsed
/// (the index is handed to the start) and `--pack-index`, and `more`.
pub(super) fn flags_of(more: &[String]) -> FrontendFlags {
    let mut argv: Vec<String> = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--state-store=postgres:///kbf?host=/run/postgresql",
        "--pack-index",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    argv.extend(more.iter().cloned());
    let matches = command().try_get_matches_from(argv).expect("flags");
    parse(&matches).expect("a frontend's flags")
}

/// The pack flags for the S3 service at `endpoint`, with a credentials file in `dir`.
pub(super) fn pack_flags(endpoint: &str, dir: &Path) -> Vec<String> {
    let file = dir.join("pack-credentials.json");
    std::fs::write(&file, r#"{"access_key_id": "k", "secret_access_key": "s"}"#).expect("written");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
    vec![
        format!("--pack-store-endpoint={endpoint}"),
        "--pack-store-bucket=kbf-fake".to_owned(),
        format!("--pack-store-credentials-file={}", file.display()),
        "--pack-writer=fe9".to_owned(),
    ]
}

pub(super) fn index() -> Arc<dyn PackIndex> {
    Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())))
}

/// Starts the frontend `flags` describe over `index`, bound and not yet serving.
pub(super) fn started(
    flags: &FrontendFlags,
    index: Option<Arc<dyn PackIndex>>,
) -> Result<FrontendHandle, Box<dyn Error>> {
    let shutdown = boxed(|| Ok(pending::<()>()));
    Prepared::new(flags)?.start(None, index, shutdown, None)
}

async fn connect(addr: SocketAddr) -> ContentAddressableStorageClient<Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
    ContentAddressableStorageClient::new(endpoint.connect().await.expect("connects"))
}

fn proto(digest: &Digest) -> reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// The status code of each item of a BatchUpdateBlobs of `blobs`.
pub(super) async fn upload(addr: SocketAddr, blobs: &[(Digest, &[u8])]) -> Vec<i32> {
    let requests = blobs
        .iter()
        .map(|(digest, bytes)| batch_update_blobs_request::Request {
            digest: Some(proto(digest)),
            data: bytes.to_vec(),
            compressor: reapi::compressor::Value::Identity as i32,
        })
        .collect();
    let response = connect(addr)
        .await
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests,
            ..BatchUpdateBlobsRequest::default()
        })
        .await
        .expect("answered");
    let responses = response.into_inner().responses;
    responses
        .iter()
        .map(|r| r.status.as_ref().expect("a status").code)
        .collect()
}

/// The text `GET /metrics` of the operator API at `addr` answers.
pub(super) async fn scrape(addr: SocketAddr) -> String {
    get(addr, "/metrics").await
}

/// The whole answer (head and body) of `GET path` on the operator listener at `addr`.
async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connects");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("sent");
    let mut text = String::new();
    stream.read_to_string(&mut text).await.expect("answered");
    text
}

fn index_form(digest: &Digest) -> BlobDigest {
    BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    }
}

/// Asserts what a frontend started with the pack flags does: it passes its start check on the
/// store, keeps a BatchUpdateBlobs in one pack of the store and in the index, answers
/// FindMissingBlobs and BatchReadBlobs from them while its storage node is unreachable, and
/// counts it all on `GET /metrics`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_the_pack_flags_keeps_uploads_in_packs() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("pack-frontend");
    let flags = flags_of(&pack_flags(s3.endpoint(), dir.path()));
    let index = index();
    let handle = started(&flags, Some(Arc::clone(&index))).expect("starts");
    assert!(handle.packs.is_some());
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let blobs: [(Digest, &[u8]); 2] = [
        (kbf_segments::sha256(b"packed one"), b"packed one"),
        (kbf_segments::sha256(b"packed two"), b"packed two"),
    ];

    assert_eq!(upload(front, &blobs).await, [0, 0]);

    let keys = s3.keys();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("cas/f/fe9/"), "{keys:?}");
    let asked = [index_form(&blobs[0].0), index_form(&blobs[1].0)];
    let located = index.locate(&asked).expect("located");
    assert!(located.iter().all(Option::is_some));
    let digests = blobs.iter().map(|(digest, _)| proto(digest)).collect();
    let missing = connect(front)
        .await
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: digests,
            ..FindMissingBlobsRequest::default()
        })
        .await
        .expect("answered");
    assert!(missing.into_inner().missing_blob_digests.is_empty());
    let read = connect(front)
        .await
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: blobs.iter().map(|(digest, _)| proto(digest)).collect(),
            ..BatchReadBlobsRequest::default()
        })
        .await
        .expect("answered");
    for (got, (_, bytes)) in read.into_inner().responses.iter().zip(&blobs) {
        assert_eq!(got.status.as_ref().expect("a status").code, 0);
        assert_eq!(got.data.as_slice(), *bytes);
    }
    let text = scrape(api).await;
    assert!(text.contains("kbf_pack_writes_total 1\n"), "{text}");
    assert!(text.contains("kbf_pack_reads_total 2\n"), "{text}");
    served.abort();
}

/// Asserts that a frontend given `--pack-index` and no pack store flags keeps nothing in packs:
/// it carries no packs, an upload goes to its storage node (which is not there, so it fails
/// UNAVAILABLE) and leaves no row, and its metrics have no pack family.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_the_pack_flags_keeps_nothing_in_packs() {
    let flags = flags_of(&[]);
    let index = index();
    let handle = started(&flags, Some(Arc::clone(&index))).expect("starts");
    assert!(handle.packs.is_none());
    assert!(handle.pack_index().is_some());
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let blob = (kbf_segments::sha256(b"not packed"), &b"not packed"[..]);

    let codes = upload(front, &[blob]).await;
    assert_ne!(codes, [Code::Ok as i32], "the node is not there");

    let located = index.locate(&[index_form(&blob.0)]).expect("located");
    assert!(located[0].is_none());
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_"), "{text}");
    served.abort();
}

/// Whether the `Welcome` that the worker listener at `worker` sends a daemon says that the
/// frontend takes `CommitPack`.
async fn welcome_says_commit(worker: SocketAddr) -> bool {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let hello = Hello {
        protocol_version: 1,
        node_id: "node-a".to_owned(),
        capabilities: vec![
            entry("os", "linux"),
            entry("arch", "x86_64"),
            entry("cpus", "1"),
            entry("mem_gib", "1"),
            entry("drivers", "fake"),
        ],
        ..Hello::default()
    };
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let first = DaemonMessage {
        message: Some(daemon_message::Message::Hello(hello)),
    };
    tx.unbounded_send(first).expect("the stream is open");
    let endpoint = Endpoint::from_shared(format!("http://{worker}")).expect("endpoint");
    let channel = endpoint.connect().await.expect("connects");
    let session = WorkerClient::new(channel).session(rx).await;
    let mut inbound = session.expect("a session").into_inner();
    let wait = Duration::from_secs(30);
    let message = tokio::time::timeout(wait, inbound.message()).await;
    let message = message.expect("a message in time").expect("a message");
    match message.expect("the stream stays open").message {
        Some(server_message::Message::Welcome(welcome)) => welcome.pack_commit,
        other => panic!("expected Welcome, got {other:?}"),
    }
}

/// Asserts that a frontend tells its daemons in `Welcome` that it takes `CommitPack` when it has
/// the pack index and the pack store, and does not when it has the index alone: it could not read
/// a pack to check it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn welcome_says_a_frontend_takes_commits_only_with_the_pack_store() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("pack-welcome");
    let with = flags_of(&pack_flags(s3.endpoint(), dir.path()));
    let with = started(&with, Some(index())).expect("starts");
    let without = started(&flags_of(&[]), Some(index())).expect("starts");
    let (with_addr, without_addr) = (with.worker, without.worker);
    let served = [tokio::spawn(with.serve()), tokio::spawn(without.serve())];

    assert!(welcome_says_commit(with_addr).await);
    assert!(!welcome_says_commit(without_addr).await);

    for task in served {
        task.abort();
    }
}

/// A daemon's session on a worker listener: the stream it sends on and the one it listens on.
struct Daemon {
    tx: UnboundedSender<DaemonMessage>,
    inbound: Streaming<ServerMessage>,
    seq: u64,
}

impl Daemon {
    /// A session for `node` at `addr`, past its Hello and the server's Welcome.
    async fn open(addr: SocketAddr, node: &str) -> Self {
        let capability = |key: &str, value: &str| Capability {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let hello = Hello {
            protocol_version: 1,
            node_id: node.to_owned(),
            capabilities: vec![
                capability("cpus", "4"),
                capability("mem_gib", "8"),
                capability("arch", "x86_64"),
                capability("os", "linux"),
                capability("drivers", "fake"),
            ],
            instance_id: format!("{node}-daemon"),
            ..Hello::default()
        };
        let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("endpoint");
        let channel = endpoint.connect().await.expect("connects");
        let (tx, rx) = unbounded();
        let first = DaemonMessage {
            message: Some(daemon_message::Message::Hello(hello)),
        };
        tx.unbounded_send(first).expect("queued");
        let response = WorkerClient::new(channel).session(rx).await;
        let mut inbound = response.expect("a session").into_inner();
        let welcome = inbound
            .message()
            .await
            .expect("healthy")
            .expect("a message");
        let ok = matches!(welcome.message, Some(server_message::Message::Welcome(_)));
        assert!(ok, "{welcome:?}");
        Self {
            tx,
            inbound,
            seq: 0,
        }
    }

    /// Sends a heartbeat carrying `pack_reads` and waits for the server's acknowledgement of it.
    async fn beat(&mut self, pack_reads: Option<PackReadCounts>) {
        self.beat_counted(pack_reads, None).await;
    }

    /// [`Self::beat`], carrying `pack_writes` too.
    async fn beat_counted(
        &mut self,
        pack_reads: Option<PackReadCounts>,
        pack_writes: Option<PackWriteCounts>,
    ) {
        self.send_beat(Heartbeat {
            pack_reads,
            pack_writes,
            ..Heartbeat::default()
        })
        .await;
    }

    /// [`Self::beat`], carrying the input cache counts `counts` alone.
    async fn beat_input_cache(&mut self, counts: Option<InputCacheCounts>) {
        self.send_beat(Heartbeat {
            input_cache: counts,
            ..Heartbeat::default()
        })
        .await;
    }

    /// Sends `beat`, numbered, and waits for the server's acknowledgement of it.
    async fn send_beat(&mut self, mut beat: Heartbeat) {
        self.seq += 1;
        beat.seq = self.seq;
        let message = DaemonMessage {
            message: Some(daemon_message::Message::Heartbeat(beat)),
        };
        self.tx.unbounded_send(message).expect("queued");
        loop {
            let received = self.inbound.message().await;
            let message = received.expect("healthy").expect("a message");
            if let Some(server_message::Message::HeartbeatAck(ack)) = message.message {
                assert_eq!(ack.seq, self.seq);
                return;
            }
        }
    }
}

/// Asserts that a frontend puts the pack-read counts its daemons send on their heartbeats on
/// `GET /metrics`, as the `kbf_pack_daemon_*` counters of the node that sent them: none of them
/// is there before a daemon has sent any, a node that sent none has no sample, the newest counts
/// of a node are the ones shown, and a node whose next heartbeat carries none loses its samples.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_the_pack_reads_its_daemons_report() {
    let flags = flags_of(&[]);
    let handle = started(&flags, Some(index())).expect("starts");
    let (worker, api) = (handle.worker, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_daemon_"), "{text}");

    let mut reading = Daemon::open(worker, "node-a").await;
    let mut plain = Daemon::open(worker, "node-b").await;
    let counts = PackReadCounts {
        served: 5,
        served_bytes: 4096,
        elsewhere: 2,
        unasked: 1,
        lost: 3,
        damaged: 4,
    };
    reading.beat(Some(counts)).await;
    plain.beat(None).await;
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
    reading.beat(Some(newer)).await;
    let text = scrape(api).await;
    let sample = "kbf_pack_daemon_served_total{node=\"node-a\"} 6\n";
    assert!(text.contains(sample), "{text}");

    reading.beat(None).await;
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_daemon_"), "{text}");
    served.abort();
}

/// Asserts that a frontend puts the input cache counts its daemons send on their heartbeats on
/// `GET /metrics` as the `kbf_node_input_cache_*` counters of the node that sent them, and on
/// `GET /v1/nodes/{id}` as its `input_cache` object: none of either before a daemon has sent
/// any, a node that sent none has no sample and no object, the newest counts of a node are the
/// ones shown, and a node whose next heartbeat carries none loses both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_the_input_cache_counts_its_daemons_report() {
    let flags = flags_of(&[]);
    let handle = started(&flags, Some(index())).expect("starts");
    let (worker, api) = (handle.worker, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let text = scrape(api).await;
    assert!(!text.contains("kbf_node_input_cache_"), "{text}");

    let mut cached = Daemon::open(worker, "node-a").await;
    let mut plain = Daemon::open(worker, "node-b").await;
    let counts = InputCacheCounts {
        fills: 5,
        fill_bytes: 4096,
        hits: 7,
        hit_bytes: 2048,
        evictions: 2,
        evicted_bytes: 512,
        trees: Some(TreeCounts {
            builds: 1,
            hits: 3,
            evictions: 0,
        }),
    };
    cached.beat_input_cache(Some(counts)).await;
    plain.beat_input_cache(None).await;
    let text = scrape(api).await;
    for (family, want) in [
        ("fills", 5),
        ("fill_bytes", 4096),
        ("hits", 7),
        ("hit_bytes", 2048),
        ("evictions", 2),
        ("evicted_bytes", 512),
        ("tree_builds", 1),
        ("tree_hits", 3),
        ("tree_evictions", 0),
    ] {
        let header = format!("# TYPE kbf_node_input_cache_{family}_total counter\n");
        assert!(text.contains(&header), "{header}{text}");
        let sample = format!("kbf_node_input_cache_{family}_total{{node=\"node-a\"}} {want}\n");
        assert!(text.contains(&sample), "{sample}{text}");
    }
    assert!(
        !text.contains("input_cache_fills_total{node=\"node-b\"}"),
        "{text}"
    );

    let detail = get(api, "/v1/nodes/node-a").await;
    let shown = r#""input_cache":{"fills":5,"fill_bytes":4096,"hits":7,"hit_bytes":2048,"evictions":2,"evicted_bytes":512,"trees":{"builds":1,"hits":3,"evictions":0}}"#;
    assert!(detail.contains(shown), "{detail}");
    let silent = get(api, "/v1/nodes/node-b").await;
    assert!(silent.starts_with("HTTP/1.1 200"), "{silent}");
    assert!(!silent.contains("input_cache"), "{silent}");

    let newer = InputCacheCounts { hits: 8, ..counts };
    cached.beat_input_cache(Some(newer)).await;
    let text = scrape(api).await;
    let sample = "kbf_node_input_cache_hits_total{node=\"node-a\"} 8\n";
    assert!(text.contains(sample), "{text}");

    cached.beat_input_cache(None).await;
    let text = scrape(api).await;
    assert!(!text.contains("kbf_node_input_cache_"), "{text}");
    let detail = get(api, "/v1/nodes/node-a").await;
    assert!(!detail.contains("input_cache"), "{detail}");
    served.abort();
}

/// Asserts that a frontend writes `kbf_execute_failed_actions_total` on `GET /metrics` with a
/// series of each of the four classes at 0 before any Execute, the page the beta gate reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_its_failed_actions_by_class() {
    let flags = flags_of(&[]);
    let handle = started(&flags, Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());
    let text = scrape(api).await;
    assert!(
        text.contains("# TYPE kbf_execute_failed_actions_total counter\n"),
        "{text}"
    );
    for class in ["farm", "request", "action", "ambiguous"] {
        let sample = format!("kbf_execute_failed_actions_total{{class=\"{class}\"}} 0\n");
        assert!(text.contains(&sample), "{sample}{text}");
    }
    served.abort();
}

/// Asserts that a frontend writes `kbf_scheduler_core` on `GET /metrics` for the core its
/// farm runs: 1 for it and 0 for the other, the page the flip gate reads. The core is the one
/// `--scheduler-core` names (legacy when it is not given), whatever core the build's tests
/// default to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_the_scheduler_core_it_runs() {
    for (given, running, other) in [
        (None, "legacy", "js"),
        (Some("--scheduler-core=legacy"), "legacy", "js"),
        (Some("--scheduler-core=js"), "js", "legacy"),
    ] {
        let more: Vec<String> = given.into_iter().map(str::to_owned).collect();
        let flags = flags_of(&more);
        let handle = started(&flags, Some(index())).expect("starts");
        let api = handle.api.expect("the operator API");
        let served = tokio::spawn(handle.serve());
        let text = scrape(api).await;
        assert!(text.contains("# TYPE kbf_scheduler_core gauge\n"), "{text}");
        let on = format!("kbf_scheduler_core{{core=\"{running}\"}} 1\n");
        let off = format!("kbf_scheduler_core{{core=\"{other}\"}} 0\n");
        assert!(text.contains(&on), "{given:?}: {on}{text}");
        assert!(text.contains(&off), "{given:?}: {off}{text}");
        served.abort();
    }
}

/// Asserts that a frontend puts the pack-write counts its daemons send on their heartbeats on
/// `GET /metrics`: none before a daemon has sent any, a sample per node and per fallback reason
/// for the node that sent them, none for a node that sent read counts only, and no write sample
/// once the node's next heartbeat carries none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_exports_the_pack_writes_its_daemons_report() {
    let flags = flags_of(&[]);
    let handle = started(&flags, Some(index())).expect("starts");
    let (worker, api) = (handle.worker, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let text = scrape(api).await;
    assert!(!text.contains("kbf_pack_daemon_"), "{text}");

    let mut writing = Daemon::open(worker, "node-a").await;
    let mut reading = Daemon::open(worker, "node-b").await;
    let counts = PackWriteCounts {
        packs: 3,
        packed: 5,
        packed_bytes: 4096,
        fallback_store: 2,
        missing_indexed: 8,
        ..PackWriteCounts::default()
    };
    let reads = Some(PackReadCounts::default());
    writing.beat_counted(reads, Some(counts)).await;
    reading.beat_counted(reads, None).await;
    let text = scrape(api).await;
    for (family, want) in [
        ("packs_written", 3),
        ("blobs_packed", 5),
        ("packed_bytes", 4096),
        ("missing_indexed", 8),
        ("missing_unasked", 0),
    ] {
        let header = format!("# TYPE kbf_pack_daemon_{family}_total counter\n");
        assert!(text.contains(&header), "{header}{text}");
        let sample = format!("kbf_pack_daemon_{family}_total{{node=\"node-a\"}} {want}\n");
        assert!(text.contains(&sample), "{sample}{text}");
    }
    let family = "kbf_pack_daemon_write_fallbacks_total";
    let store = format!("{family}{{node=\"node-a\",reason=\"store\"}} 2\n");
    assert!(text.contains(&store), "{store}{text}");
    let off = format!("{family}{{node=\"node-a\",reason=\"off\"}} 0\n");
    assert!(text.contains(&off), "{off}{text}");
    let other = format!("{family}{{node=\"node-b\"");
    assert!(!text.contains(&other), "{text}");

    writing.beat_counted(reads, None).await;
    let text = scrape(api).await;
    assert!(
        !text.contains("kbf_pack_daemon_packs_written_total"),
        "{text}"
    );
    assert!(!text.contains(family), "{text}");
    served.abort();
}

/// Asserts that a frontend whose pack store cannot be written to does not serve: the start check
/// fails and `serve` says so naming the flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pack_store_that_cannot_be_written_fails_the_start_check() {
    let dir = scratch_dir("pack-unreachable");
    let flags = flags_of(&pack_flags("http://127.0.0.1:1", dir.path()));
    let handle = started(&flags, Some(index())).expect("binds");

    let failed = handle.serve().await;

    let said = failed.expect_err("not served").to_string();
    assert!(said.starts_with("--pack-store-endpoint: "), "{said}");
}

/// Asserts that pack flags with no pack index open refuse the start, naming the flag, though
/// the command line parser accepts the flags: a start is handed the index by the flag that opens
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pack_flags_without_the_pack_index_refuse_the_start() {
    let dir = scratch_dir("pack-no-index");
    let flags = flags_of(&pack_flags("http://127.0.0.1:1", dir.path()));

    let refused = started(&flags, None).err().expect("refused");

    assert!(refused.to_string().contains("--pack-index"), "{refused}");
}
