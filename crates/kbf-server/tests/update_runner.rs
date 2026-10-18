//! The controller where the frontend starts it (`docs/design/split-mode-updates.md` section
//! 6.2): with `update/policy.auto_nodes` false, the default, nothing is started however long
//! it waits; set, the next look starts the host, and an activation order reaches it on its
//! session.

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
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, QUIET, hello_on};
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
    worker: SocketAddr,
    rows: Rows,
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
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Frontend {
        api,
        worker,
        rows: Rows::new(store),
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

/// Catches a frontend that rolls the fleet by itself, and one that never does. A desired
/// release is set and a Mac is connected and idle: with `auto_nodes` false nothing is cordoned
/// and no order is sent for as long as the controller has looked many times; once it is set the
/// host is cordoned and its activation order arrives, of the epoch `Welcome` named, for the
/// desired commit.
///
/// Mutants seen red: the switch not read (the host is started with it off); the controller not
/// started by the frontend (no order with it on).
#[tokio::test]
async fn nothing_starts_until_auto_nodes_is_set() {
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
    let front_rows = front.rows.clone();
    tokio::task::spawn_blocking(move || {
        front_rows
            .offer(&Release::new(A), kbf_types::FarmTime::from_millis(1_000))
            .expect("a run");
    })
    .await
    .expect("offered");

    let activation = |m: &server_message::Message| match m {
        server_message::Message::UpdateOrder(order) => Some(order.clone()),
        _ => None,
    };
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
    assert_eq!(order.epoch, mac.epoch);
    assert_eq!(order.commit, A);
    assert_eq!(placement(front.api, "mac-1").await, "cordoned");
}
