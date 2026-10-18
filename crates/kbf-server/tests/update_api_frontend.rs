//! The frontend's own update over the operator API (`docs/design/pull-updates.md`):
//! `POST /v1/update/frontend` and `GET /v1/update/frontend`, over real listeners, the
//! `update/frontend` row of a state store and a real order spool in a directory.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_server::members::MembershipConfig;
use kbf_server::release::frontend_update::Installer;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::update::{Began, FrontendState, OrderId, Release, Rows};
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::ReleasePlatform;
use kbf_update::{OrderRecord, Verb};
use serde_json::{Value, json};
use sha2::Digest as _;
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kbf-server-release-mount-{}-{}",
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
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// The state store a server is given, and the same store for the test to write rows into.
fn state_and_store() -> (StateStore, Arc<dyn Store>) {
    let clock = Arc::new(ManualClock::default());
    let fake = Arc::new(Fake::with_flaw(clock, Flaw::None));
    let store = fake as Arc<dyn Store>;
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
    (state, store)
}

fn start(
    releases: Option<Arc<ReleaseStore>>,
    state: Option<StateStore>,
    installer: Option<Arc<Installer>>,
    dir: &Dir,
) -> Running {
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(ApiToken::from_file(&path).expect("a usable token")),
        local_cache: None,
        releases,
        installer,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        kbf_auth::Policy::allow_all(),
        state,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind");
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Running {
        api,
        stop: Some(stop),
    }
}

/// One request; the status and the JSON body (`null` for none).
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: bool,
    content_type: Option<&str>,
    body: &[u8],
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if auth {
        head.push_str(&format!("Authorization: Bearer {TOKEN}\r\n"));
    }
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(body).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text[9..12].parse().expect("a status");
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";
const X86: &str = "linux-x86_64";
const ARM: &str = "linux-arm64";
/// The wall clock the route reads, at the start of a test, in milliseconds.
const T0: u64 = 1_800_000_000_000;

/// A frontend host: the server, the state store it runs over, the release store, the
/// installer's directory and the clock the route reads.
struct Host {
    running: Running,
    store: Arc<dyn Store>,
    dir: Dir,
    clock: Arc<AtomicU64>,
}

impl Host {
    fn orders(&self) -> PathBuf {
        self.dir.0.join("installer").join("orders")
    }

    fn status(&self) -> PathBuf {
        self.dir.0.join("installer").join("status.json")
    }

    /// The order records in the spool (a `.partial-` file is not one).
    fn records(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.orders())
            .expect("the spool")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    fn rows(&self) -> Rows {
        Rows::new(Arc::clone(&self.store))
    }

    fn row(&self) -> Option<kbf_server::update::FrontendRow> {
        self.rows().frontend().expect("the row").value
    }

    fn advance(&self, by: Duration) {
        self.clock.fetch_add(
            u64::try_from(by.as_millis()).expect("ms"),
            Ordering::Relaxed,
        );
    }

    /// The installer's word, as it writes `status.json` (schema 2).
    fn say(&self, state: &str, installed: Option<&str>) {
        let installed = installed.map_or("null".to_owned(), |c| format!("\"{c}\""));
        let text = format!(
            "{{\"schema\":2,\"installed_commit\":{installed},\"previous_commit\":null,\
             \"staged_commit\":null,\"state\":\"{state}\",\"refusal\":\"\",\"acked_order_id\":0,\
             \"staged\":null}}\n"
        );
        std::fs::write(self.status(), text).expect("status.json");
    }

    /// The installer takes the record out of the spool, as it does once it has read it.
    fn consume_records(&self) {
        for name in self.records() {
            std::fs::remove_file(self.orders().join(name)).expect("remove");
        }
    }

    async fn post(&self, commit: &str) -> (u16, Value) {
        let body = json!({ "commit": commit }).to_string();
        http(
            self.running.api,
            "POST",
            "/v1/update/frontend",
            true,
            Some("application/json"),
            body.as_bytes(),
        )
        .await
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        http(self.running.api, "GET", path, false, None, b"").await
    }

    /// Store `commit` for `platform`, and its attestation when `attested`.
    async fn put(&self, commit: &str, platform: &str, attested: bool) {
        let path = format!("/v1/releases/{commit}/{platform}/kbf");
        let api = self.running.api;
        assert_eq!(
            http(api, "PUT", &path, true, None, b"a tarball").await.0,
            201
        );
        if attested {
            let at = format!("{path}.attestation");
            assert_eq!(
                http(api, "PUT", &at, true, None, b"{\"bundle\":1}").await.0,
                201
            );
        }
    }
}

/// A frontend host on `platform`, with whichever of the three flags' halves are asked for.
fn host_with(releases: bool, state: bool, installer: bool, platform: ReleasePlatform) -> Host {
    let dir = Dir::new();
    let (state_store, store) = state_and_store();
    let clock = Arc::new(AtomicU64::new(T0));
    let releases = releases.then(|| {
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"))
    });
    let installer = installer.then(|| {
        let mut installer = Installer::open(
            &dir.0.join("installer").join("orders"),
            Some(&dir.0.join("installer").join("status.json")),
        )
        .expect("the installer's spool");
        installer.platform = Some(platform);
        let now = Arc::clone(&clock);
        installer.now_ms = Box::new(move || now.load(Ordering::Relaxed));
        Arc::new(installer)
    });
    let running = start(releases, state.then_some(state_store), installer, &dir);
    Host {
        running,
        store,
        dir,
        clock,
    }
}

fn host() -> Host {
    host_with(true, true, true, ReleasePlatform::LinuxX86_64)
}

/// Catches a second update started while one is in flight, and two orders where one was
/// asked: the first POST is 202 with the order's numbers, the second (another commit) is 409
/// `frontend_busy` naming the first, the spool holds exactly one record, and the row is the
/// first's, open.
///
/// Mutant planted: the busy answer dropped (the open row is overwritten by a second claim).
#[tokio::test]
async fn a_second_update_while_one_is_open_is_busy_and_only_one_record_exists() {
    let h = host();
    h.put(A, X86, true).await;
    h.put(B, X86, true).await;
    let (code, first) = h.post(A).await;
    assert_eq!(code, 202, "{first}");
    assert_eq!(
        (first["generation"].clone(), first["commit"].clone()),
        (json!(1), json!(A))
    );
    let (code, busy) = h.post(B).await;
    assert_eq!(code, 409, "{busy}");
    assert_eq!(busy["error"], "frontend_busy");
    assert_eq!(busy["order_id"], first["order_id"]);
    assert_eq!(h.records().len(), 1, "one record: {:?}", h.records());
    let row = h.row().expect("a row");
    assert_eq!(
        (row.commit.as_str(), row.state, row.generation),
        (A, FrontendState::Open, 1)
    );
    // The same commit again is as busy as another one.
    assert_eq!(h.post(A).await.0, 409);
    assert_eq!(h.records().len(), 1);
}

/// Catches an open row that nothing ever closes: a crash after the row and before the record
/// leaves an open row and no record. It holds the route (409) until the deadline of 3600 s
/// and no longer: at 3599 s still busy, at 3600 s the row is closed and a new update (the
/// next generation) is ordered, with one record.
///
/// Mutant planted: the deadline left out of the settled check (the row holds for ever).
#[tokio::test]
async fn a_crash_between_the_row_and_the_record_closes_at_the_deadline() {
    let h = host();
    h.put(A, X86, true).await;
    // What a crash leaves: the row opened (as the route does first), no record.
    let opened = h
        .rows()
        .begin_frontend(&Release::new(A), 7, T0)
        .expect("begin");
    assert!(matches!(opened, Began::Opened(_)));
    assert!(h.records().is_empty());
    assert_eq!(h.post(A).await.0, 409, "the row is open");
    h.advance(Duration::from_secs(3599));
    assert_eq!(h.post(A).await.0, 409, "a second early");
    h.advance(Duration::from_secs(1));
    let (code, next) = h.post(A).await;
    assert_eq!(code, 202, "{next}");
    assert_eq!(next["generation"], 2, "the next generation");
    assert_eq!(h.records().len(), 1);
}

/// Catches a release complete for another platform being taken for this host's: the store
/// holds `linux-arm64` complete and `linux-x86_64` with no attestation, on an x86_64
/// frontend, so the route is 409 naming the host's platform, writes no row and no record;
/// attested for x86_64 it is ordered, and the record carries the x86_64 tarball.
///
/// Mutant planted: the platform filter left out (any complete platform is taken: the arm64
/// release is ordered on an x86_64 host).
#[tokio::test]
async fn a_release_incomplete_for_this_hosts_platform_is_409_while_another_is_complete() {
    let h = host();
    h.put(A, ARM, true).await;
    h.put(A, X86, false).await;
    let (code, body) = h.post(A).await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body["error"].as_str().expect("text").contains(X86),
        "{body}"
    );
    assert!(h.row().is_none(), "no row for a refused request");
    assert!(h.records().is_empty());
    let at = format!("/v1/releases/{A}/{X86}/kbf.attestation");
    assert_eq!(
        http(h.running.api, "PUT", &at, true, None, b"{\"bundle\":1}")
            .await
            .0,
        201
    );
    assert_eq!(h.post(A).await.0, 202);
}

/// Catches a write that skips a gate or a body that is not what it says: no token is 401, a
/// body not typed as JSON is 415 (both with no body at all too), a commit that is not 40
/// lowercase hex, a body with another field, and a body that is not JSON are 400, and none of
/// them writes a row or a record.
#[tokio::test]
async fn the_frontend_update_is_a_gated_json_write_that_writes_nothing_when_refused() {
    let h = host();
    h.put(A, X86, true).await;
    let body = json!({ "commit": A }).to_string();
    let send = |auth: bool, ct: Option<&'static str>, body: Vec<u8>| {
        let api = h.running.api;
        async move {
            http(api, "POST", "/v1/update/frontend", auth, ct, &body)
                .await
                .0
        }
    };
    let json_type = Some("application/json");
    assert_eq!(send(false, json_type, body.clone().into_bytes()).await, 401);
    assert_eq!(send(true, None, body.clone().into_bytes()).await, 415);
    assert_eq!(send(true, Some("text/plain"), body.into_bytes()).await, 415);
    assert_eq!(
        send(true, json_type, b"{\"commit\":\"XYZ\"}".to_vec()).await,
        400
    );
    let upper = json!({ "commit": A.to_uppercase() }).to_string();
    assert_eq!(send(true, json_type, upper.into_bytes()).await, 400);
    let extra = json!({ "commit": A, "expect_generation": 1 }).to_string();
    assert_eq!(send(true, json_type, extra.into_bytes()).await, 400);
    assert_eq!(send(true, json_type, b"not json".to_vec()).await, 400);
    assert!(h.row().is_none());
    assert!(h.records().is_empty());
}

/// Catches a record the installer would not read, or bytes that drift from the contract: the
/// file is named for the order's (epoch, id), is exactly the golden bytes of a
/// `frontend_update` record (the ones `kbf-update` holds), and parses with
/// `OrderRecord::parse`, the parser the installer reads with, to the verb `FrontendUpdate`
/// with the stored tarball's digest and size, the attestation as text and no expected facts.
#[tokio::test]
async fn the_record_is_the_golden_bytes_and_parses_with_the_installers_parser() {
    let h = host();
    h.put(A, X86, true).await;
    let (code, order) = h.post(A).await;
    assert_eq!(code, 202, "{order}");
    let (epoch, id) = (
        order["epoch"].as_u64().expect("epoch"),
        order["order_id"].as_u64().expect("id"),
    );
    assert_eq!(h.records(), vec![kbf_update::record::file_name(epoch, id)]);
    let bytes = std::fs::read(h.orders().join(h.records().remove(0))).expect("the record");
    let sha = hex::encode(sha2::Sha256::digest(b"a tarball"));
    let golden = format!(
        "{{\"order_id\":{id},\"epoch\":{epoch},\"generation\":1,\"verb\":\"frontend_update\",\
         \"commit\":\"{A}\",\"window_s\":600,\"components\":[{{\"name\":\"kbf\",\
         \"sha256\":\"{sha}\",\"size\":9,\"attestation\":\"{{\\\"bundle\\\":1}}\"}}]}}"
    );
    assert_eq!(String::from_utf8(bytes.clone()).expect("text"), golden);
    let record = OrderRecord::parse(&bytes).expect("the installer's parser takes it");
    assert_eq!(record.verb, Verb::FrontendUpdate);
    assert!(record.expected.is_none());
    assert_eq!(
        (record.order_id, record.epoch, record.components[0].size),
        (id, epoch, 9)
    );
    let mode = std::fs::metadata(h.orders().join(record.file_name()))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Catches a failed write leaving the route wedged or the row lying: with the spool
/// unwritable (its directory replaced by a file: no user, root included, can write into it)
/// the answer is 500, no record exists, and the row is **closed**, so the next request, once
/// the spool is back, is ordered at the next generation without waiting for a deadline.
///
/// Mutant planted: the record written before the row is opened (a failed write then leaves
/// no row at all, and the generation is not drawn).
#[tokio::test]
async fn an_unwritable_spool_is_500_and_the_row_is_closed() {
    let h = host();
    h.put(A, X86, true).await;
    std::fs::remove_dir_all(h.orders()).expect("remove the spool");
    std::fs::write(h.orders(), b"not a directory").expect("a file in its place");
    let (code, body) = h.post(A).await;
    assert_eq!(code, 500, "{body}");
    let row = h.row().expect("the row was written first");
    assert_eq!((row.state, row.generation), (FrontendState::Closed, 1));
    let (_, shown) = h.get("/v1/update/frontend").await;
    assert_eq!(shown["row"]["state"], "closed", "{shown}");
    std::fs::remove_file(h.orders()).expect("remove the file");
    std::fs::create_dir(h.orders()).expect("the spool is back");
    let (code, again) = h.post(A).await;
    assert_eq!(code, 202, "{again}");
    assert_eq!(again["generation"], 2);
    assert_eq!(h.records().len(), 1);
}

/// Catches an answer made up for a server that cannot keep the row, hold the release or reach
/// the installer: each missing piece is 503 naming its own flag, for the write, and the read of
/// the frontend update says it is off; the frontend section of `GET /v1/update` says so too.
#[tokio::test]
async fn without_a_state_store_a_release_store_or_an_installer_the_route_names_the_flag() {
    for (releases, state, installer, flag) in [
        (true, false, true, "--state-store"),
        (false, true, true, "--releases-dir"),
        (true, true, false, "--installer-orders-dir"),
    ] {
        let h = host_with(releases, state, installer, ReleasePlatform::LinuxX86_64);
        let (code, body) = h.post(A).await;
        assert_eq!(code, 503, "{flag}: {body}");
        assert!(
            body["error"].as_str().expect("text").contains(flag),
            "{body}"
        );
        if !installer || !state {
            assert_eq!(h.get("/v1/update/frontend").await.0, 503, "{flag}");
        }
    }
    let h = host_with(true, true, false, ReleasePlatform::LinuxX86_64);
    let (code, view) = h.get("/v1/update").await;
    assert_eq!(code, 200);
    assert_eq!(view["frontend"]["enabled"], false, "{view}");
}

/// Catches a row closed by something other than the installer finishing this order: with the
/// record still in the spool, whatever the status says, the route is busy; with the record
/// gone and the installer still fetching, busy; with it gone and a confirm of some other
/// commit (a node order), busy; with it gone, nothing in flight and the commit installed, the
/// row is closed and the next update is ordered; `reverted` and `refused` with the record gone
/// close it too.
///
/// Mutant planted: the record-in-spool test dropped from the settled check.
#[tokio::test]
async fn the_row_closes_when_the_installer_has_finished_with_the_order() {
    let h = host();
    for commit in [A, B] {
        h.put(commit, X86, true).await;
    }
    assert_eq!(h.post(A).await.0, 202);
    h.say("confirmed", Some(A));
    assert_eq!(h.post(B).await.0, 409, "the record is still in the spool");
    h.consume_records();
    h.say("fetching", Some(B));
    assert_eq!(h.post(B).await.0, 409, "still fetching");
    h.say("activated_unconfirmed", Some(A));
    assert_eq!(h.post(B).await.0, 409, "on trial");
    h.say("confirmed", Some(B));
    assert_eq!(
        h.post(B).await.0,
        409,
        "a confirm of another commit is not ours"
    );
    h.say("confirmed", Some(A));
    let (code, next) = h.post(B).await;
    assert_eq!(code, 202, "{next}");
    assert_eq!(next["generation"], 2);
    // Refused and reverted are ends of the update too.
    for word in ["refused", "reverted"] {
        h.consume_records();
        h.say(word, Some(A));
        assert_eq!(h.post(A).await.0, 202, "{word}");
    }
}

/// Catches a rollback that does not outrank what it undoes: A, then B, then A again are
/// generations 1, 2 and 3, and the order ids only grow.
#[tokio::test]
async fn a_rollback_is_an_older_commit_under_a_higher_generation() {
    let h = host();
    for commit in [A, B] {
        h.put(commit, X86, true).await;
    }
    let mut seen = Vec::new();
    for commit in [A, B, A] {
        let (code, order) = h.post(commit).await;
        assert_eq!(code, 202, "{order}");
        seen.push((
            order["generation"].as_u64().expect("generation"),
            order["order_id"].as_u64().expect("id"),
        ));
        h.consume_records();
        h.say("confirmed", Some(commit));
    }
    assert_eq!(seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(seen.windows(2).all(|w| w[0].1 < w[1].1), "{seen:?}");
}

/// Catches a view that hides the row or the installer: `GET /v1/update/frontend` is open,
/// shows the open row (order, generation, commit, whether the record is still in the spool)
/// and the installer's status read from `status.json`, `GET /v1/update` carries the same in
/// its `frontend` section, and neither closes the row (a read changes nothing).
#[tokio::test]
async fn the_views_show_the_row_and_the_installers_status_and_change_nothing() {
    let h = host();
    h.put(A, X86, true).await;
    let (_, before) = h.get("/v1/update/frontend").await;
    assert_eq!(before["row"], Value::Null, "{before}");
    assert_eq!(before["installer"]["present"], false);
    let (_, order) = h.post(A).await;
    h.consume_records();
    h.say("confirmed", Some(A));
    for path in ["/v1/update/frontend", "/v1/update"] {
        let (code, view) = h.get(path).await;
        assert_eq!(code, 200, "{path}");
        let section = if path == "/v1/update" {
            &view["frontend"]
        } else {
            &view
        };
        assert_eq!(section["enabled"], true);
        assert_eq!(section["row"]["order_id"], order["order_id"], "{section}");
        assert_eq!(
            (
                section["row"]["state"].clone(),
                section["row"]["record_in_spool"].clone(),
                section["installer"]["state"].clone(),
                section["installer"]["installed_commit"].clone(),
            ),
            (json!("open"), json!(false), json!("confirmed"), json!(A)),
            "{section}"
        );
    }
    assert_eq!(
        h.row().expect("row").state,
        FrontendState::Open,
        "GET closes nothing"
    );
}

/// Catches two requests that race for the one place both getting it: eight threads open the
/// row at once over one store, and exactly one is `Opened`, the rest `Busy` naming its order.
///
/// Mutant planted: the row written unconditionally (`Cond::Any`) with the busy check kept
/// (a thread that read before another wrote overwrites it).
#[test]
fn racing_claims_of_the_row_are_not_both_given_it() {
    let (_, store) = state_and_store();
    let rows = Rows::new(store);
    let outcomes: Vec<Began> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let rows = rows.clone();
                scope.spawn(move || {
                    let commit = if i % 2 == 0 { A } else { B };
                    rows.begin_frontend(&Release::new(commit), 7, T0)
                        .expect("begin")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });
    let opened: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            Began::Opened(row) => Some(row.clone()),
            Began::Busy(_) => None,
        })
        .collect();
    assert_eq!(opened.len(), 1, "{outcomes:?}");
    for busy in outcomes.iter().filter_map(|o| match o {
        Began::Busy(row) => Some(row),
        Began::Opened(_) => None,
    }) {
        assert_eq!(busy.order, opened[0].order);
    }
}

/// Catches the row's bytes moving: a row is frozen across one step of a rolling restart, so
/// the bytes of one open row (tags 1 to 6: order id, epoch, generation, commit, state,
/// written-at) are held here, and `end_frontend` for another order leaves the row alone.
#[test]
fn the_row_bytes_are_frozen_and_ending_another_order_changes_nothing() {
    let (_, store) = state_and_store();
    let rows = Rows::new(Arc::clone(&store));
    let Began::Opened(row) = rows
        .begin_frontend(&Release::new(A), 7, 1234)
        .expect("begin")
    else {
        panic!("a first claim is opened");
    };
    assert_eq!((row.order.epoch, row.order.seq, row.generation), (7, 1, 1));
    let mut want = vec![0x08, 1, 0x10, 7, 0x18, 1, 0x22, 40];
    want.extend_from_slice(A.as_bytes());
    want.extend_from_slice(&[0x28, 1, 0x30, 0xd2, 0x09]);
    assert_eq!(
        store
            .get("update/frontend")
            .expect("get")
            .expect("row")
            .value,
        want
    );
    assert!(!rows.end_frontend(OrderId::new(7, 99)).expect("end"));
    assert!(!rows.end_frontend(OrderId::new(8, 1)).expect("end"));
    assert_eq!(rows.frontend().expect("row").value, Some(row.clone()));
    assert!(rows.end_frontend(row.order).expect("end"));
    assert!(
        !rows
            .end_frontend(row.order)
            .expect("a second end is a no-op")
    );
}
