//! `GET /v1/denied` over real listeners: the deny list with its reasons, behind the gates
//! every operator write passes (`docs/api.md`). It runs on the store's fake, and on the
//! server over a store the test chooses; no SQLite.

#[path = "support/hooked.rs"]
mod hooked;
mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_server::members::{Members, MembershipConfig};
use kbf_server::token::ApiToken;
use kbf_server::verbs::routes;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{Clock as _, Cond, ManualClock, Store, Txn};
use kbf_types::operator_api::DeniedList;
use support::{HELLO_WAIT, INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const OPERATOR: &str = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\nContent-Type: application/json\r\n";

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 1,
    }
}

/// A new empty directory of this test binary's, under the target directory, removed when it drops.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-server-operator-verbs-{name}"),
    )
}

fn token() -> ApiToken {
    let dir = scratch("token");
    let path = dir.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
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
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        scheduler_core: kbf_server::SchedulerCore::under_test(),
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        usage_profiles: kbf_server::UsageProfiles::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// A whole server over the store's fake; its stop switch is dropped with it.
struct Server {
    api: SocketAddr,
    clock: Arc<ManualClock>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start() -> Server {
    let clock = Arc::new(ManualClock::default());
    let state = StateStore {
        store: store_on(Arc::clone(&clock)),
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(token()),
        local_cache: None,
    };
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
    let api = bound.api.expect("an API address");
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server {
        api,
        clock,
        stop: Some(stop),
    }
}

fn live_store() -> Arc<dyn Store> {
    store_on(Arc::new(ManualClock::default()))
}

fn store_on(clock: Arc<ManualClock>) -> Arc<dyn Store> {
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

/// Just the route over `members`, on a listener of their own.
async fn serve_routes(token: Option<ApiToken>, members: Option<Arc<Members>>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    // As `serve_api` does: the handler reads the gate's `Trust`.
    let app = routes(token, members).layer(axum::middleware::from_fn_with_state(
        None::<Arc<kbf_server::proxy_auth::ProxySecret>>,
        kbf_server::proxy_auth::gate_requests,
    ));
    tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.expect("serve");
    });
    addr
}

fn live_members() -> Arc<Members> {
    Arc::new(Members::new(live_store(), config()))
}

struct Answer {
    code: u16,
    head: String,
    body: String,
}

async fn request(api: SocketAddr, method: &str, path: &str, headers: &str, body: &str) -> Answer {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         {headers}Connection: close\r\n\r\n{body}"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    Answer {
        code,
        head: head.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

fn assert_no_store(answer: &Answer) {
    assert!(
        answer.head.contains("cache-control: no-store"),
        "{}",
        answer.head
    );
}

async fn deny(api: SocketAddr, entry: &str, reason: &str) {
    let body = serde_json::json!({ "entry": entry, "reason": reason }).to_string();
    let answer = request(api, "POST", "/v1/denied", OPERATOR, &body).await;
    assert_eq!(answer.code, 200, "{}", answer.body);
}

async fn listed(api: SocketAddr) -> DeniedList {
    let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);
    serde_json::from_str(&answer.body).expect("a deny list")
}

fn names(list: &DeniedList) -> Vec<(&str, &str)> {
    list.denied
        .iter()
        .map(|d| (d.entry.as_str(), d.reason.as_str()))
        .collect()
}

/// Catches a deny list that cannot be read back: `GET /v1/denied` must list every entry kind
/// (serial, key hash, node) with the reason it was added with and a time (the store's clock at the denial), in the order the
/// list sorts, as `POST` stored it (canonical spelling), and drop an entry that `DELETE`
/// took off. It is served by the real server, so it also pins that the new `GET` shares the
/// path with `POST` and `DELETE` (a merge that lost one method would answer 405). The empty
/// list is `{"denied":[]}`, never 404, and every answer is `no-store`.
///
/// Catches: the listing reading only `node` rows; the reason left out (empty); the
/// route merged so that `GET` was never reached (405); `at_ms` a constant; `no_store` missing
/// from the 401/403/415, 404, 503 and 500 answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deny_list_is_read_back_with_every_kind_and_reason() {
    let server = start();
    let api = server.api;
    let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 200, "{}", answer.body);
    assert_eq!(answer.body, r#"{"denied":[]}"#);
    assert_no_store(&answer);
    assert!(answer.head.contains("application/json"), "{}", answer.head);

    // The store's clock moves between the denials, so each entry has its own time.
    let mut times = Vec::new();
    let hash = "ab".repeat(32);
    for (entry, reason) in [
        ("node Mac-07".to_owned(), "retired"),
        ("serial 0A:1B".to_owned(), "a lost laptop"),
        (format!("spki-sha256 {hash}"), "leaked key"),
    ] {
        server.clock.advance(Duration::from_secs(7));
        times.push(u64::try_from(server.clock.now().as_duration().as_millis()).expect("ms"));
        deny(api, &entry, reason).await;
    }

    let list = listed(api).await;
    assert_eq!(
        names(&list),
        [
            ("serial a1b", "a lost laptop"),
            (&format!("spki-sha256 {hash}")[..], "leaked key"),
            ("node mac-07", "retired"),
        ]
    );
    // Listed serial (denied second), spki (third), node (first).
    let at: Vec<u64> = list.denied.iter().map(|d| d.at_ms).collect();
    assert_eq!(at, [times[1], times[2], times[0]], "{list:?}");

    let gone = request(api, "DELETE", "/v1/denied?entry=serial+a1b", OPERATOR, "").await;
    assert_eq!(gone.code, 200, "{}", gone.body);
    let list = listed(api).await;
    assert_eq!(
        names(&list),
        [
            (&format!("spki-sha256 {hash}")[..], "leaked key"),
            ("node mac-07", "retired"),
        ]
    );
}

/// Catches the listing answering a caller that fails a gate, or answering a gate failure
/// with something other than the gate's own status: no token and a wrong token (401), a
/// browser page's `Origin` (403), a request without `Content-Type: application/json` (415,
/// on a `GET` too, as for `/v1/enroll`), and a server started without `--api-token-file`
/// (403). A refused request lists no entry, though one is denied.
///
/// Catches: the `gate` call removed from the listing; the content-type check
/// skipped on the `GET`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listing_passes_the_write_gate() {
    let members = live_members();
    members
        .deny(&"node mac-07".parse().expect("an entry"), "SECRET-REASON")
        .expect("deny");
    let api = serve_routes(Some(token()), Some(Arc::clone(&members))).await;
    let wrong = "Authorization: Bearer nope-nope-nope-nope-nope-nope-nope-nope\r\nContent-Type: application/json\r\n";
    let no_type = "Authorization: Bearer kbf-test-token-0123456789abcdef0123456789\r\n";
    let browser = format!("{OPERATOR}Origin: https://example.com\r\n");
    for (what, headers, want) in [
        ("no token", "Content-Type: application/json\r\n", 401),
        ("a wrong token", wrong, 401),
        ("an Origin", browser.as_str(), 403),
        ("no content type", no_type, 415),
    ] {
        let answer = request(api, "GET", "/v1/denied", headers, "").await;
        assert_eq!(answer.code, want, "with {what}: {}", answer.body);
        assert_no_store(&answer);
        assert!(
            !answer.body.contains("SECRET-REASON"),
            "{what}: {}",
            answer.body
        );
    }
    let off = serve_routes(None, Some(members)).await;
    let answer = request(off, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 403, "writes off: {}", answer.body);
    assert_no_store(&answer);
}

/// Catches a server without members answering anything but 404 (after the gate), and a
/// 500 for a missing store.
///
/// Catches: the missing-members answer a 500.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_without_members_answers_404() {
    let none = serve_routes(Some(token()), None).await;
    let answer = request(none, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 404, "{}", answer.body);
    assert_no_store(&answer);
}

/// Catches a listing that assumes it is the only frontend: two frontends over one store
/// (two `Members` on the same rows, no writer role between them) each list an entry the
/// other denied, at once.
///
/// Catches: the listing read from a copy taken at start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_frontends_over_one_store_list_each_others_entries() {
    let store = live_store();
    let a = Arc::new(Members::new(Arc::clone(&store), config()));
    let b = Arc::new(Members::new(store, config()));
    let (api_a, api_b) = (
        serve_routes(Some(token()), Some(Arc::clone(&a))).await,
        serve_routes(Some(token()), Some(Arc::clone(&b))).await,
    );
    a.deny(&"node one".parse().expect("entry"), "by a")
        .expect("deny");
    b.deny(&"node two".parse().expect("entry"), "by b")
        .expect("deny");
    for api in [api_a, api_b] {
        assert_eq!(
            names(&listed(api).await),
            [("node one", "by a"), ("node two", "by b")]
        );
    }
}

/// Catches a listing that answers a store that cannot be read with something other than a
/// fixed status and text: a store that fails is 503, a row that is no entry or does not
/// decode is 500 (the list is never guessed at), and a store call that panics is 500, each
/// without the store's own message or the row's bytes in the answer.
///
/// Catches: the store's error text echoed (`injected` in the body); a corrupt row
/// skipped instead of refused (200 with the other entries); the panic left to drop the
/// connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_that_cannot_read_the_store_says_so_in_fixed_words() {
    use std::sync::atomic::Ordering;
    let store = hooked::Hooked::new();
    let members = Arc::new(Members::new(Arc::clone(&store) as Arc<dyn Store>, config()));
    let api = serve_routes(Some(token()), Some(members)).await;

    store.fail_scans.store(1, Ordering::SeqCst);
    let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 503, "{}", answer.body);
    assert_no_store(&answer);
    assert!(
        answer.body.contains("the state store failed"),
        "{}",
        answer.body
    );
    assert!(!answer.body.contains("injected"), "{}", answer.body);

    store.panic_scans.store(1, Ordering::SeqCst);
    let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
    assert_eq!(answer.code, 500, "{}", answer.body);
    assert_no_store(&answer);
    assert!(
        answer.body.contains("the listing task failed"),
        "{}",
        answer.body
    );

    for (key, value) in [
        ("deny/node/mac-07", &b"\xff\xff-SECRET-ROW"[..]),
        ("deny/not-an-entry", &b""[..]),
    ] {
        store
            .fake
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("commit");
        let answer = request(api, "GET", "/v1/denied", OPERATOR, "").await;
        assert_eq!(answer.code, 500, "{key}: {}", answer.body);
        assert_no_store(&answer);
        assert!(
            answer.body.contains("a stored row is unreadable") && !answer.body.contains("SECRET"),
            "{key}: {}",
            answer.body
        );
        store
            .fake
            .txn(Txn::new().delete(key, Cond::Any))
            .expect("store")
            .expect("commit");
    }
    assert_eq!(listed(api).await, DeniedList::default());
}
