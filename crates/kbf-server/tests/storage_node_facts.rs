//! What a storage node tells its frontend of the build it runs and of its metadata log
//! (`RegisterRequest` fields 10 to 12, `HeartbeatRequest` fields 24 to 26): the newest log
//! version its build runs (informational), the version the log is at now, and the commit
//! built. These tests run the node's own registration path (`Joiner`, and `start_storage`
//! through to the open of its log), so a node that stops saying them, or says a constant,
//! goes red here and not only in the tests that call the membership service directly.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_log::MachineVersion;
use kbf_map::MemberId;
use kbf_meta::Retention;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_server::{Membership, MembershipServer};
use kbf_server::join::{Identity as NodeIdentity, Joiner, reported_commit};
use kbf_server::members::{Members, MembershipConfig, NodeFacts};
use kbf_server::raft_log::RUNS;
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_server::{EpochPolicy, Listeners, RaftMetaLog, StateStore, bind_server_with_state};
use kbf_state::Store;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Endpoint, Server};

const MS: Duration = Duration::from_millis(1);

/// A TTL of 60 ms (beats every 20 ms) on a clock that moves only when a test moves it.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: 60 * MS,
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

/// A frontend over a new fake state store: its worker address and its members.
fn frontend() -> (SocketAddr, Members) {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
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
    let members = Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config());
    (worker, members)
}

fn flags(dir: &Path, worker: SocketAddr) -> StorageFlags {
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("cold").display()),
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

/// What a request said: `(runs_log, log_version, build_commit)`.
type Said = (Option<u32>, Option<u32>, Option<String>);

/// What a node sent to a membership service.
#[derive(Default)]
struct Recorder {
    registered: Mutex<Vec<pb::RegisterRequest>>,
    beats: Mutex<Vec<pb::HeartbeatRequest>>,
}

impl Recorder {
    fn registrations(&self) -> Vec<Said> {
        let requests = self.registered.lock().expect("lock");
        requests
            .iter()
            .map(|r| (r.runs_log, r.log_version, r.build_commit.clone()))
            .collect()
    }

    fn heartbeats(&self) -> Vec<Said> {
        let requests = self.beats.lock().expect("lock");
        requests
            .iter()
            .map(|r| (r.runs_log, r.log_version, r.build_commit.clone()))
            .collect()
    }
}

/// A membership service that admits every node as member 1 at boot 1 and keeps what the node
/// sent.
struct Recording(Arc<Recorder>);

#[tonic::async_trait]
impl Membership for Recording {
    async fn register(
        &self,
        request: tonic::Request<pb::RegisterRequest>,
    ) -> Result<tonic::Response<pb::RegisterResponse>, tonic::Status> {
        let mut registered = self.0.registered.lock().expect("lock");
        registered.push(request.into_inner());
        Ok(tonic::Response::new(pb::RegisterResponse {
            member_id: 1,
            boot: 1,
            heartbeat_interval_ms: 1,
            ..Default::default()
        }))
    }

    async fn heartbeat(
        &self,
        request: tonic::Request<pb::HeartbeatRequest>,
    ) -> Result<tonic::Response<pb::HeartbeatResponse>, tonic::Status> {
        let mut beats = self.0.beats.lock().expect("lock");
        beats.push(request.into_inner());
        Ok(tonic::Response::new(pb::HeartbeatResponse::default()))
    }
}

/// The recording service on a port of its own.
fn serve(seen: &Arc<Recorder>) -> SocketAddr {
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr: SocketAddr = incoming.local_addr().expect("addr");
    let service = MembershipServer::new(Recording(Arc::clone(seen)));
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    addr
}

/// Catches: a joiner that does not put what its node runs, its log's version and its build in
/// the registration and in every heartbeat it sends, or sends a constant, or reads the log's
/// version once instead of at each call, or says 0 for what it cannot say. A joiner given
/// `RUNS` and a handle at version 1 registers saying `RUNS`, 1 and its commit; after the
/// handle moves to 4, its next beat says 4; a joiner given no log says neither number (absent,
/// not 0) and still says its commit. Catches: `log_version` read when the joiner is built;
/// `runs_log` or `log_version` a constant; `log_facts` answering `Some` without a log.
#[tokio::test]
async fn a_joiner_says_what_its_node_runs_and_where_its_log_is_at_every_call() {
    let seen = Arc::new(Recorder::default());
    let addr = serve(&seen);
    let version = Arc::new(AtomicU32::new(MachineVersion::V1.get()));
    let mut with = joiner(addr).with_log(RUNS.get(), Arc::clone(&version));
    let joined = with.register().await.expect("registers");
    with.beat(&joined).await.expect("beats");
    version.store(MachineVersion::V4.get(), Ordering::Relaxed);
    with.beat(&joined).await.expect("beats");
    let mut without = joiner(addr);
    let joined = without.register().await.expect("registers");
    without.beat(&joined).await.expect("beats");

    let commit = Some(reported_commit().to_owned());
    let runs = Some(RUNS.get());
    let (one, four) = (Some(1), Some(4));
    let said = seen.registrations();
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[0], (runs, one, commit.clone()));
    assert_eq!(said[1], (None, None, commit.clone()));
    let said = seen.heartbeats();
    assert_eq!(said.len(), 3, "{said:?}");
    assert_eq!(said[0], (runs, one, commit.clone()));
    assert_eq!(said[1], (runs, four, commit.clone()));
    assert_eq!(said[2], (None, None, commit));
}

/// Asserts that a started storage node reports what it runs and where its log is: its
/// frontend's row holds `RUNS` for what its build runs, the commit it was built from, and
/// the version its log is at. The log in the node's directory was raised to version 3 by an
/// earlier build, and a registered node raises nothing as it opens (`open_registered`), so
/// the row reads 3: not 1 (the version of a new log, which the registration would say for a
/// constant) and not `RUNS` (5). A node that reported nothing would leave the numbers
/// absent.
#[tokio::test]
async fn a_started_node_reports_what_it_runs_and_the_version_its_log_is_at() {
    let (worker, members) = frontend();
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "node-facts-started");
    let log = RaftMetaLog::open_dir(&dir.join("meta"), Retention::default(), "kbf/")
        .await
        .expect("open the log");
    log.raise_machine_version(MachineVersion::V3)
        .expect("the earlier build's raise");
    drop(log);
    let node = start_storage(&flags(&dir, worker), never)
        .await
        .expect("starts");
    let serving = tokio::spawn(node.serve());
    let want = NodeFacts {
        runs_log: Some(RUNS.get()),
        log_version: Some(MachineVersion::V3.get()),
        build_commit: Some(reported_commit().to_owned()),
    };
    let mut seen = NodeFacts::default();
    for _ in 0..1000 {
        let mut all = members.node_facts().expect("rows");
        seen = all.remove(&MemberId(1)).unwrap_or_default();
        if seen == want {
            break;
        }
        tokio::time::sleep(5 * MS).await;
    }
    serving.abort();
    assert_eq!(seen, want);
}
