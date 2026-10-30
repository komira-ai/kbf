//! The report a storage node makes when it registers: the member and boot its metadata log
//! last wrote as (`kbf_server::members::LastBoot`). These tests run the node's own
//! registration path (`Joiner::register`, and `start_storage` through to the open of its
//! log) against a real frontend over the fake state store, so a node that stops sending the
//! report goes red here and not only in the tests that call the membership service directly.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_server::join::{Identity as NodeIdentity, Joiner};
use kbf_server::members::{LastBoot, MembershipConfig};
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_server::{EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::Store;
use tonic::transport::Endpoint;

const MS: Duration = Duration::from_millis(1);

/// A frontend over a new fake state store: its worker address and the store.
fn frontend() -> (SocketAddr, Arc<Hooked>) {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(3600),
            dwell: Duration::from_secs(7200),
            replication: 2,
        },
        history: None,
    };
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        scheduler_core: kbf_server::SchedulerCore::under_test(),
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners,
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    let worker = bound.worker;
    tokio::spawn(bound.serving);
    (worker, store)
}

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-register-boot")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

fn flags(dir: &Path, cold: &str, worker: SocketAddr) -> StorageFlags {
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join(cold).display()),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", dir.join("meta").display()),
        format!("--frontend=http://{worker}"),
    ];
    parse(&command().try_get_matches_from(argv).expect("flags")).expect("parsed")
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

fn joiner(worker: SocketAddr) -> Joiner {
    let channel = Endpoint::from_shared(format!("http://{worker}"))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "node-x".to_owned(),
        addr: "192.0.2.5:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    Joiner::new(channel, who, 5 * MS)
}

/// Catches: a joiner that does not put its log's report in the registration it sends
/// (defect: `last_boot: 0` and `last_member_id: 0` in `Joiner::register`, or no
/// `with_last_boot`). A node whose log last wrote as member 1 at boot 4 registers with a
/// fresh store and is answered member 1 at boot 5; without the report it is boot 1.
#[tokio::test]
async fn a_joiner_registers_with_the_member_and_boot_its_log_last_wrote_as() {
    let (worker, _store) = frontend();
    let mut with = joiner(worker).with_last_boot(LastBoot::from_wire(1, 4));
    let got = with.register().await.expect("registers");
    assert_eq!((got.member_id, got.boot), (1, 5));

    let (worker, _store) = frontend();
    let none = joiner(worker).register().await.expect("registers");
    assert_eq!((none.member_id, none.boot), (1, 1), "no report, no raise");
}

/// Catches: a node that does not send its log's report when it starts (defect: no
/// `.with_last_boot(last)` in `open_cache`). The node runs against one frontend, so its log
/// holds member 1 at boot 1 (the cold store is another directory: the first node still holds
/// its lock); the frontend's store is then lost and the node starts against a
/// new one, which counts from 1 again. With the report the node is given boot 2 and opens;
/// without it, boot 1, which its log refuses and the start fails.
#[tokio::test]
async fn a_node_started_over_its_old_log_tells_a_new_frontend_what_the_log_wrote() {
    let dir = dir("restart");
    let (first, _old) = frontend();
    let node = start_storage(&flags(&dir, "cold-1", first), never)
        .await
        .expect("starts against the first frontend");
    let serving = tokio::spawn(node.serve());
    tokio::time::sleep(50 * MS).await;
    serving.abort();
    let _ = serving.await;

    let (second, store) = frontend();
    let node = start_storage(&flags(&dir, "cold-2", second), never).await;
    assert!(node.is_ok(), "{:?}", node.err());
    let boot = store
        .fake
        .get("member/1")
        .expect("read")
        .map(|row| row.version);
    assert!(
        boot.is_some(),
        "the node registered as member 1 with the new store"
    );
}
