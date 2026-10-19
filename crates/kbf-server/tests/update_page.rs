//! The Updates page of `kbf-ui` over this server's real routes: `GET /v1/update`,
//! `GET /v1/nodes` and `GET /v1/update/hosts` answer the documents the page parses, with
//! and without the per-host detail, and what a host sent reaches the page as text.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::update::UpdateStatus;
use kbf_proto::update::update_status::State;
use kbf_proto::worker::daemon_message;
use kbf_server::members::MembershipConfig;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::update::{Release, Rows};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::FarmTime;
use kbf_ui::{Fetched, Source, SourceError};
use serde_json::json;
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, hello};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const A: &str = "0123456789abcdef0123456789abcdef01234567";

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kbf-server-update-page-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("a test directory");
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
        platform_aliases: Default::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

struct Running {
    api: SocketAddr,
    worker: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// A server with a release store, a state store (returned, to write rows into) and
/// `policy`.
fn start(policy: kbf_auth::Policy, dir: &Dir) -> (Running, Arc<dyn Store>) {
    let clock = Arc::new(ManualClock::default());
    let store = Arc::new(Fake::with_flaw(clock, Flaw::None)) as Arc<dyn Store>;
    let state = StateStore {
        store: Arc::clone(&store),
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
    };
    let releases =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(ApiToken::from_file(&path).expect("a usable token")),
        local_cache: None,
        releases: Some(releases),
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        policy,
        Some(state),
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let (api, worker) = (bound.api.expect("an API address"), bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    (
        Running {
            api,
            worker,
            stop: Some(stop),
        },
        store,
    )
}

/// One request: the status and the raw body.
async fn http(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\
         Authorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(body).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header end");
    let status = String::from_utf8_lossy(&raw[9..12])
        .parse()
        .expect("a status");
    (status, raw[split + 4..].to_vec())
}

/// The operator API as the page's source.
struct Over<'a>(&'a Running);

impl Source for Over<'_> {
    async fn get(&self, url: &str) -> Result<Fetched, SourceError> {
        let (status, body) = http(self.0.api, "GET", url, b"").await;
        Ok(Fetched {
            status,
            body,
            headers: Vec::new(),
        })
    }
}

/// Stores a complete linux release of `commit`: its tarball and attestation.
async fn put(running: &Running, commit: &str) {
    let path = format!("/v1/releases/{commit}/linux-x86_64/kbf");
    assert_eq!(http(running.api, "PUT", &path, b"a tarball").await.0, 201);
    let attestation = format!("{path}.attestation");
    assert_eq!(
        http(running.api, "PUT", &attestation, b"{\"bundle\":1}")
            .await
            .0,
        201
    );
}

async fn page(running: &Running) -> (u16, String) {
    let answer = kbf_ui::respond(&Over(running), "/ui/updates", "").await;
    (
        answer.status(),
        String::from_utf8(answer.body().to_vec()).expect("UTF-8"),
    )
}

/// Catches: a key the page reads that the routes do not answer, or answer in another
/// form (the page would be `502`); the detail columns missing under a policy that
/// allows `farm.get`; a host's refusal, a string from outside, written as markup; a
/// farm paused through its policy row not shown paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_page_renders_from_the_real_routes_with_the_detail() {
    let dir = Dir::new();
    let farm_get =
        json!({"authenticationPolicy": {"allow": {}}, "farm": {"getAuthorizer": {"allow": {}}}});
    let policy = kbf_auth::Policy::from_json(&farm_get.to_string()).expect("a policy");
    let (running, rows) = start(policy, &dir);
    put(&running, A).await;
    let rows = Rows::new(rows);
    rows.offer(&Release::new(A), FarmTime::from_millis(1))
        .expect("offer");
    rows.set_paused(true).expect("pause");
    let daemon = FakeDaemon::connect(running.worker, hello("node-a", 1, 1))
        .await
        .expect("registered");
    daemon.send(daemon_message::Message::UpdateStatus(UpdateStatus {
        running_commit: A[..12].to_owned(),
        state: State::Refused as i32,
        refusal: "<img src=x onerror=alert(1)>".to_owned(),
        ..UpdateStatus::default()
    }));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let html = loop {
        let (status, html) = page(&running).await;
        assert_eq!(status, 200, "{html}");
        if html.contains("refused") {
            break html;
        }
        assert!(tokio::time::Instant::now() < deadline, "{html}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    for text in [
        &format!("<dd>{A} (generation 1)</dd>"),
        "<span class=\"pill fail\">paused</span>",
        "linux-x86_64: complete (kbf)",
        "<th scope=\"col\">Last error</th>",
        "&lt;img src=x onerror=alert(1)&gt;",
        "<td>node-a</td>",
    ] {
        assert!(html.contains(text), "{text}: {html}");
    }
    assert!(!html.contains("<img"), "{html}");
}

/// Catches: the page failing, or guessing, where the per-host route is off (no policy
/// names `farm.getAuthorizer`: the real answer is `404`): the open summary stays and the
/// detail columns are absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_farm_get_authority_the_page_shows_the_open_summary_only() {
    let dir = Dir::new();
    let (running, rows) = start(kbf_auth::Policy::allow_all(), &dir);
    put(&running, A).await;
    Rows::new(rows)
        .offer(&Release::new(A), FarmTime::from_millis(1))
        .expect("offer");
    let _daemon = FakeDaemon::connect(running.worker, hello("node-a", 1, 1))
        .await
        .expect("registered");
    let (status, html) = page(&running).await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("the detail route answered 404"), "{html}");
    assert!(html.contains("<td>node-a</td>"), "{html}");
    assert!(
        !html.contains("<th scope=\"col\">Last error</th>"),
        "{html}"
    );
    assert!(
        html.contains(&format!("<dd>{A} (generation 1)</dd>")),
        "{html}"
    );
    assert!(
        html.contains("<span class=\"pill ok\">running</span>"),
        "{html}"
    );
}
