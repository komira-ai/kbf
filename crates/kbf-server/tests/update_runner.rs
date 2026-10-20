//! The controller where the frontend starts it (`docs/design/split-mode-updates.md` section
//! 6.2): with no `update/policy` written, or `auto_nodes` true, the next look after the
//! operator's desired write starts the host and an activation order reaches it on its
//! session; paused, or with `auto_nodes` written false, nothing is started however long it
//! waits.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::update::UpdateStatus;
use kbf_proto::worker::{daemon_message, server_message};
use kbf_server::members::MembershipConfig;
use kbf_server::update::{Policy, Release, Rows};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::Store;
use support::{Client, FakeDaemon, HELLO_WAIT, INTERVAL, Job, QUIET, hello, hello_on};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const A: &str = "0123456789abcdef0123456789abcdef01234567";

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: INTERVAL,
        hello_wait: HELLO_WAIT,
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

struct Frontend {
    api: SocketAddr,
    reapi: SocketAddr,
    worker: SocketAddr,
    rows: Rows,
    hooked: Arc<hooked::Hooked>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Frontend {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start() -> Frontend {
    let store = hooked::Hooked::new();
    let state = StateStore {
        store: store.clone() as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
    };
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: None,
        local_cache: None,
        releases: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        Some(state),
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let (api, worker) = (bound.api.expect("an API address"), bound.worker);
    let reapi = bound.reapi;
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Frontend {
        api,
        reapi,
        worker,
        rows: Rows::new(store.clone()),
        hooked: store,
        stop: Some(stop),
    }
}

async fn placement(api: SocketAddr, node: &str) -> String {
    let mut stream = TcpStream::connect(api).await.expect("connect");
    let request = "GET /v1/nodes HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (_, body) = response.split_once("\r\n\r\n").expect("a body");
    let nodes: serde_json::Value = serde_json::from_str(body).expect("JSON");
    let listed = nodes["nodes"]
        .as_array()
        .and_then(|all| all.iter().find(|n| n["node_id"] == node))
        .expect("the node is listed");
    listed["placement"]["state"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

/// A Mac that is connected, idle and reports an old release.
async fn idle_mac(front: &Frontend) -> FakeDaemon {
    let mac = FakeDaemon::connect(
        front.worker,
        hello_on("mac-1", 4, 8, &[("arch", "arm64"), ("os", "macos")]),
    )
    .await
    .expect("registered");
    mac.send(daemon_message::Message::UpdateStatus(UpdateStatus {
        running_commit: "fedcba987654".to_owned(),
        ..UpdateStatus::default()
    }));
    mac
}

fn activation(m: &server_message::Message) -> Option<kbf_proto::update::UpdateOrder> {
    match m {
        server_message::Message::UpdateOrder(order) => Some(order.clone()),
        _ => None,
    }
}

async fn offer(front: &Frontend) {
    let rows = front.rows.clone();
    tokio::task::spawn_blocking(move || {
        rows.offer(&Release::new(A), kbf_types::FarmTime::from_millis(1_000))
            .expect("a run");
    })
    .await
    .expect("offered");
}

/// Catches a frontend that never starts a run: no `update/policy` row was ever written, and
/// the operator's write of a desired release is the whole click. The next look cordons the
/// idle Mac and its activation order arrives, of the epoch `Welcome` named, for the desired
/// commit. (Before this an unwritten policy read `auto_nodes` false and nothing could set it.)
///
/// Mutants seen red: an unwritten policy read as `auto_nodes` false (the runner reading
/// `policy_if_set`, or `Policy::default` false); the controller not started by the frontend.
#[tokio::test]
async fn a_desired_release_with_no_policy_written_starts_the_idle_host() {
    let front = start();
    let mut mac = idle_mac(&front).await;
    assert!(
        front.rows.policy_if_set().expect("readable").is_none(),
        "the test needs a policy that was never written"
    );
    assert!(Policy::default().auto_nodes);
    offer(&front).await;
    let order = mac.expect("an activation order", activation).await;
    assert_eq!(order.epoch, mac.epoch);
    assert_eq!(order.commit, A);
    assert_eq!(placement(front.api, "mac-1").await, "cordoned");
}

/// Catches a pause that does not stop, and a resume that turns the farm off. The operator
/// pauses before any policy exists (which writes the row), then offers a release: nothing is
/// cordoned and no order is sent however long the controller looks. Resumed, the run starts.
///
/// Mutants seen red: `paused` not read by the controller (the Mac is started while paused);
/// a pause that writes the policy with `auto_nodes` false (the run never starts after the
/// resume).
#[tokio::test]
async fn pause_stops_a_run_and_resume_starts_it_without_a_policy_of_the_operators() {
    let front = start();
    let mut mac = idle_mac(&front).await;
    let rows = front.rows.clone();
    tokio::task::spawn_blocking(move || rows.set_paused(true).expect("paused"))
        .await
        .expect("set");
    offer(&front).await;
    assert!(mac.expect_within(QUIET, activation).await.is_none());
    assert_eq!(placement(front.api, "mac-1").await, "serving");

    let rows = front.rows.clone();
    tokio::task::spawn_blocking(move || rows.set_paused(false).expect("resumed"))
        .await
        .expect("set");
    let order = mac.expect("an activation order", activation).await;
    assert_eq!(order.commit, A);
    assert_eq!(placement(front.api, "mac-1").await, "cordoned");
}

/// Catches a switch that is not read: an operator who wrote `auto_nodes` false (and a floor,
/// to show the rest of the row is read) has nothing started however long the controller looks,
/// and writing it true starts the run.
///
/// Mutant seen red: the switch not read (the host is started with it off).
#[tokio::test]
async fn auto_nodes_written_false_starts_nothing_until_it_is_written_true() {
    let front = start();
    let mut mac = idle_mac(&front).await;
    let rows = front.rows.clone();
    tokio::task::spawn_blocking(move || {
        rows.set_policy(&Policy {
            auto_nodes: false,
            max_hosts_out: 2,
            ..Policy::default()
        })
        .expect("policy");
    })
    .await
    .expect("set");
    offer(&front).await;
    assert!(mac.expect_within(QUIET, activation).await.is_none());
    assert_eq!(placement(front.api, "mac-1").await, "serving");

    let rows = front.rows.clone();
    tokio::task::spawn_blocking(move || {
        rows.set_policy(&Policy {
            auto_nodes: true,
            ..Policy::default()
        })
        .expect("policy");
    })
    .await
    .expect("set");
    let order = mac.expect("an activation order", activation).await;
    assert_eq!(order.commit, A);
}

/// Catches a farm whose load is not read: a Mac holds a lease on a running action, another action
/// waits in the queue, and a Linux host is idle beside them. With `auto_nodes` set and a release
/// desired nothing is started (a host with a storage half is never started, and the queue
/// makes the last Mac of its class untouchable), nothing is cordoned and no order is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_farm_with_work_queued_starts_nothing() {
    let front = start();
    let client = Client::connect(front.reapi, front.worker).await;
    let mut mac = FakeDaemon::connect(
        front.worker,
        hello_on("mac-1", 4, 8, &[("arch", "arm64"), ("os", "macos")]),
    )
    .await
    .expect("registered");
    let mut linux = FakeDaemon::connect(front.worker, hello("linux-1", 4, 8))
        .await
        .expect("registered");
    let (running, waiting) = (
        Job::new("running", &[("OSFamily", "macos"), ("ISA", "arm64")]),
        // No host runs this: it waits in the queue.
        Job::new("waiting", &[("OSFamily", "linux"), ("ISA", "arm64")]),
    );
    client.upload(&running.blobs()).await;
    client.upload(&waiting.blobs()).await;
    let _first = client.execute(&running.action).await;
    mac.start().await;
    let _second = client.execute(&waiting.action).await;
    arm(&front).await;
    assert!(mac.expect_within(QUIET, order).await.is_none());
    assert!(linux.expect_within(QUIET, order).await.is_none());
    assert_eq!(placement(front.api, "mac-1").await, "serving");
    assert_eq!(placement(front.api, "linux-1").await, "serving");
}

/// Catches a loaded host read as idle: two Macs, one running an action and nothing queued.
/// The idle one is started and the busy one is left alone.
///
/// Mutant seen red: the leases read as none (the busy Mac is idle, and first by name).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_idle_mac_is_started_and_the_busy_one_is_not() {
    let front = start();
    let client = Client::connect(front.reapi, front.worker).await;
    let mac_of = |name: &'static str| {
        let worker = front.worker;
        async move {
            FakeDaemon::connect(
                worker,
                hello_on(name, 4, 8, &[("arch", "arm64"), ("os", "macos")]),
            )
            .await
            .expect("registered")
        }
    };
    let mut busy = mac_of("mac-1").await;
    let mut idle = mac_of("mac-2").await;
    let running = Job::new("running", &[("OSFamily", "macos"), ("ISA", "arm64")]);
    client.upload(&running.blobs()).await;
    let _op = client.execute(&running.action).await;
    // The action lands on one of them; the other is the idle one.
    let (busy, idle) = tokio::select! {
        _ = busy.start() => (&mut busy, &mut idle),
        _ = idle.start() => (&mut idle, &mut busy),
    };
    for node in [&*busy, &*idle] {
        node.send(daemon_message::Message::UpdateStatus(UpdateStatus {
            running_commit: "fedcba987654".to_owned(),
            ..UpdateStatus::default()
        }));
    }
    arm(&front).await;
    let got = idle
        .expect("an activation order for the idle Mac", order)
        .await;
    assert_eq!(got.commit, A);
    assert!(busy.expect_within(QUIET, order).await.is_none());
}

fn order(m: &server_message::Message) -> Option<kbf_proto::update::UpdateOrder> {
    match m {
        server_message::Message::UpdateOrder(order) => Some(order.clone()),
        _ => None,
    }
}

/// Sets a release desired and `auto_nodes`.
async fn arm(front: &Frontend) {
    let rows = front.rows.clone();
    tokio::task::spawn_blocking(move || {
        rows.offer(&Release::new(A), kbf_types::FarmTime::from_millis(1_000))
            .expect("a run");
        rows.set_policy(&Policy {
            auto_nodes: true,
            ..Policy::default()
        })
        .expect("policy");
    })
    .await
    .expect("set");
}

/// Catches a controller whose look panics taking the frontend with it, or one that goes on
/// after it lost its state: the first read of the policy panics (a store that crashes in its
/// blocking task); the controller stops, with a line saying so, and the frontend still
/// answers, with nothing started for the release and `auto_nodes` set after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_look_that_panics_stops_the_controller_and_not_the_frontend() {
    let front = start();
    let mut mac = FakeDaemon::connect(
        front.worker,
        hello_on("mac-1", 4, 8, &[("arch", "arm64"), ("os", "macos")]),
    )
    .await
    .expect("registered");
    mac.send(daemon_message::Message::UpdateStatus(UpdateStatus {
        running_commit: "fedcba987654".to_owned(),
        ..UpdateStatus::default()
    }));
    front.hooked.after_get("update/policy", || {
        panic!("a store that crashes");
    });
    tokio::time::sleep(QUIET).await;
    arm(&front).await;
    assert!(mac.expect_within(QUIET, order).await.is_none());
    assert_eq!(placement(front.api, "mac-1").await, "serving");
}
