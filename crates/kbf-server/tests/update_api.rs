//! The desired release over the operator API (`docs/design/pull-updates.md` section 6.3):
//! `GET /v1/update`, `POST /v1/update/desired`, `pause` and `resume`, over real listeners and
//! the `update/desired` row of a state store.

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
use kbf_server::members::MembershipConfig;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use serde_json::{Value, json};
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

fn state() -> StateStore {
    state_and_store().0
}

/// A state over a store the test controls (it can make reads fail or panic).
fn state_over(store: Arc<dyn Store>) -> StateStore {
    StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
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

fn start(releases: Option<Arc<ReleaseStore>>, state: Option<StateStore>, dir: &Dir) -> Running {
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(ApiToken::from_file(&path).expect("a usable token")),
        local_cache: None,
        releases,
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

/// Offer `commit`: the status and body.
async fn offer(running: &Running, commit: &str) -> (u16, Value) {
    let body = json!({ "commit": commit }).to_string();
    http(
        running.api,
        "POST",
        "/v1/update/desired",
        true,
        Some("application/json"),
        body.as_bytes(),
    )
    .await
}

/// Store the tarball of `commit`, and its attestation when `attested`.
async fn put(running: &Running, commit: &str, attested: bool) {
    let path = format!("/v1/releases/{commit}/linux-x86_64/kbf");
    let (code, _) = http(running.api, "PUT", &path, true, None, b"a tarball").await;
    assert_eq!(code, 201);
    if attested {
        let (code, _) = http(
            running.api,
            "PUT",
            &format!("{path}.attestation"),
            true,
            None,
            b"{\"bundle\":1}",
        )
        .await;
        assert_eq!(code, 201);
    }
}

/// Catches a desired release set for something the hosts could not fetch and verify, a
/// repeated click starting a new run, and a rollback that does not outrank what it undoes:
/// a commit with no complete platform (no attestation yet, or nothing stored) is 409; the
/// first complete commit is generation 1; the same commit again keeps its generation; a
/// second commit is 2 and the first again, a rollback, is 3. The desired release is read
/// back by `GET /v1/update` with the stored releases.
///
/// Mutants seen red: the completeness check dropped (the unattested commit is accepted);
/// the generation not moved on a different commit.
#[tokio::test]
async fn a_complete_commit_is_offered_a_repeat_keeps_the_generation_a_rollback_outranks() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    let (code, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 200);
    assert_eq!(view["desired"], Value::Null);
    assert_eq!(offer(&running, A).await.0, 409, "nothing stored");
    put(&running, A, false).await;
    assert_eq!(offer(&running, A).await.0, 409, "no attestation yet");
    put(&running, B, true).await;
    let (code, first) = offer(&running, B).await;
    assert_eq!((code, first["generation"].clone()), (200, json!(1)));
    assert_eq!(first["commit"], B);
    assert_eq!(first["paused"], false);
    assert_eq!(
        offer(&running, B).await.1["generation"],
        1,
        "a repeat keeps it"
    );
    // A is attested at last; offered, it is a new run; B again is a rollback and outranks both.
    let path = format!("/v1/releases/{A}/linux-x86_64/kbf.attestation");
    assert_eq!(
        http(running.api, "PUT", &path, true, None, b"{\"bundle\":2}")
            .await
            .0,
        201
    );
    assert_eq!(offer(&running, A).await.1["generation"], 2);
    assert_eq!(offer(&running, B).await.1["generation"], 3);
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(view["desired"]["commit"], B);
    assert_eq!(view["desired"]["generation"], 3);
    assert_eq!(
        view["releases"]["releases"].as_array().map(Vec::len),
        Some(2),
        "{view}"
    );
}

/// Catches a write that skips a gate or a body that is not what it says: no token is 401, a
/// body that is not JSON-typed is 415, a commit that is not 40 lowercase hex or a body with
/// other fields is 400, and none of them moves the row.
#[tokio::test]
async fn the_desired_release_is_a_gated_json_write() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    put(&running, A, true).await;
    let body = json!({ "commit": A }).to_string();
    let post = |auth: bool, ct: Option<&'static str>, body: Vec<u8>| {
        let api = running.api;
        async move {
            http(api, "POST", "/v1/update/desired", auth, ct, &body)
                .await
                .0
        }
    };
    assert_eq!(
        post(false, Some("application/json"), body.clone().into_bytes()).await,
        401
    );
    assert_eq!(post(true, None, body.clone().into_bytes()).await, 415);
    assert_eq!(post(true, Some("text/plain"), body.into_bytes()).await, 415);
    assert_eq!(
        post(
            true,
            Some("application/json"),
            b"{\"commit\":\"XYZ\"}".to_vec()
        )
        .await,
        400
    );
    assert_eq!(
        post(
            true,
            Some("application/json"),
            br#"{"commit":"0123456789abcdef0123456789abcdef01234567","x":1}"#.to_vec()
        )
        .await,
        400
    );
    assert_eq!(
        post(true, Some("application/json"), b"not json".to_vec()).await,
        400
    );
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(
        view["desired"],
        Value::Null,
        "no refused write moved the row"
    );
}

/// Catches pause and resume: a run that does not exist cannot be paused (409), a paused run
/// says so in `GET /v1/update`, a new offer keeps it paused, and resume clears it; both need
/// the token.
#[tokio::test]
async fn pause_and_resume_set_the_run_flag() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    let pause = |auth: bool, which: &'static str| {
        let api = running.api;
        async move { http(api, "POST", &format!("/v1/update/{which}"), auth, None, b"").await }
    };
    assert_eq!(pause(true, "pause").await.0, 409, "no run yet");
    put(&running, A, true).await;
    assert_eq!(offer(&running, A).await.0, 200);
    assert_eq!(pause(false, "pause").await.0, 401);
    assert_eq!(pause(true, "pause").await, (200, json!({ "paused": true })));
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(view["desired"]["paused"], true);
    assert_eq!(
        pause(true, "resume").await,
        (200, json!({ "paused": false }))
    );
}

/// Catches an answer made up for a server that cannot keep the row or hold the releases:
/// without a state store the desired release is 503 naming `--state-store`, without a
/// release store it is 503 naming `--releases-dir`.
#[tokio::test]
async fn without_a_state_store_or_a_release_store_the_update_routes_say_so() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let no_rows = start(Some(Arc::clone(&store)), None, &dir);
    let (code, body) = http(no_rows.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .expect("text")
            .contains("--state-store"),
        "{body}"
    );
    assert_eq!(offer(&no_rows, A).await.0, 503);
    let no_store = start(None, Some(state()), &dir);
    let (code, body) = http(no_store.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .expect("text")
            .contains("--releases-dir"),
        "{body}"
    );
}

/// Catches a stale click replacing a newer decision: `expect_generation` makes the offer a
/// compare-and-set. A match writes (0 before any offer, then the generation shown); a
/// mismatch is 409 naming both generations and writes nothing (the commit and generation
/// stay); a repeat of the desired commit with the wrong expectation is refused too; and a
/// field the route does not know is still 400.
///
/// Mutant seen red: `expect_generation` ignored (the stale click is written).
#[tokio::test]
async fn an_offer_can_expect_the_generation_it_was_made_on() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    put(&running, A, true).await;
    put(&running, B, true).await;
    let with = |commit: &'static str, expect: u64| {
        let body = json!({ "commit": commit, "expect_generation": expect }).to_string();
        let api = running.api;
        async move {
            http(
                api,
                "POST",
                "/v1/update/desired",
                true,
                Some("application/json"),
                body.as_bytes(),
            )
            .await
        }
    };
    // Nothing offered yet: the page that saw "no run" says 0.
    let (code, first) = with(A, 1).await;
    assert_eq!(code, 409, "{first}");
    assert!(
        first["error"]
            .as_str()
            .expect("text")
            .contains("generation 0"),
        "{first}"
    );
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(
        view["desired"],
        Value::Null,
        "a refused offer wrote nothing"
    );
    assert_eq!(with(A, 0).await.1["generation"], 1);
    // The page that saw generation 1 sets B; the stale page (still on 1) then cannot set A.
    assert_eq!(with(B, 1).await.1["generation"], 2);
    let (code, stale) = with(A, 1).await;
    assert_eq!(code, 409);
    let why = stale["error"].as_str().expect("text");
    assert!(why.contains("generation 2") && why.contains('1'), "{why}");
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(
        (
            view["desired"]["commit"].as_str(),
            view["desired"]["generation"].as_u64()
        ),
        (Some(B), Some(2))
    );
    // The commit already desired, offered on a stale page: refused, not answered as a no-op.
    assert_eq!(with(B, 1).await.0, 409);
    assert_eq!(with(B, 2).await.1["generation"], 2);
    // Other fields are still refused.
    let body = json!({ "commit": A, "expect_generation": 2, "x": 1 }).to_string();
    let (code, _) = http(
        running.api,
        "POST",
        "/v1/update/desired",
        true,
        Some("application/json"),
        body.as_bytes(),
    )
    .await;
    assert_eq!(code, 400);
}

/// Catches a policy the operator set being invisible to the page, or invented when none was
/// set: `policy` is null until the row is written, then shows the row (durations in seconds).
#[tokio::test]
async fn the_policy_row_is_shown_when_it_exists() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let (state, rows_store) = state_and_store();
    let running = start(Some(store), Some(state), &dir);
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(view["policy"], Value::Null);
    let policy = kbf_server::update::Policy {
        max_hosts_out: 2,
        auto_nodes: true,
        ..kbf_server::update::Policy::default()
    };
    kbf_server::update::Rows::new(rows_store)
        .set_policy(&policy)
        .expect("write");
    let (_, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(view["policy"]["max_hosts_out"], 2);
    assert_eq!(view["policy"]["auto_nodes"], true);
    assert_eq!(view["policy"]["auto_frontend"], false);
    assert_eq!(view["policy"]["lease_limit_secs"], 900);
    assert_eq!(view["policy"]["mac_force_after_secs"], Value::Null);
}

/// Catches a store that fails turning into an answer that is not a failure: a read that
/// fails (or panics inside its blocking task) is 500 saying the rows failed, on the view, the
/// offer and pause, and a policy read that fails after the desired read succeeded is 500
/// too; nothing is written by the offer that could not read.
#[tokio::test]
async fn a_state_store_that_fails_is_a_500_not_an_empty_answer() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let hooked = hooked::Hooked::new();
    let running = start(Some(store), Some(state_over(hooked.clone())), &dir);
    put(&running, A, true).await;
    let fails = |n: usize| {
        hooked
            .fail_gets
            .store(n, std::sync::atomic::Ordering::SeqCst)
    };
    fails(1);
    assert_eq!(
        http(running.api, "GET", "/v1/update", false, None, b"")
            .await
            .0,
        500
    );
    fails(1);
    assert_eq!(offer(&running, A).await.0, 500);
    fails(1);
    let (code, _) = http(running.api, "POST", "/v1/update/pause", true, None, b"").await;
    assert_eq!(code, 500);
    // A store whose read panics inside the blocking task: a 500, not a hung request.
    hooked
        .panic_gets
        .store(1, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        http(running.api, "GET", "/v1/update", false, None, b"")
            .await
            .0,
        500
    );
    // The desired read succeeds and the policy read after it fails.
    let again = Arc::clone(&hooked);
    hooked.after_get("update/desired", move || {
        again
            .fail_gets
            .store(1, std::sync::atomic::Ordering::SeqCst);
    });
    assert_eq!(
        http(running.api, "GET", "/v1/update", false, None, b"")
            .await
            .0,
        500
    );
    // Healthy again: nothing was written meanwhile.
    let (code, view) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!((code, view["desired"].clone()), (200, Value::Null));
}

/// Catches a release store that cannot be listed answered as an empty or complete list:
/// with the index directory replaced by a file, the view and the offer are 500 and name no
/// path.
#[tokio::test]
async fn a_release_store_that_cannot_be_listed_is_a_500() {
    let dir = Dir::new();
    let releases = dir.0.join("releases");
    let store = Arc::new(ReleaseStore::open(&releases, Limits::default()).expect("open"));
    let running = start(Some(store), Some(state()), &dir);
    std::fs::remove_dir_all(releases.join("index")).expect("remove");
    std::fs::write(releases.join("index"), b"not a directory").expect("file");
    let (code, body) = http(running.api, "GET", "/v1/update", false, None, b"").await;
    assert_eq!(code, 500);
    assert!(
        !body.to_string().contains("releases"),
        "no path leaks: {body}"
    );
    assert_eq!(offer(&running, A).await.0, 500);
}

/// Catches a write that skips the 503 for a missing half: a store but no state store makes
/// pause 503 naming `--state-store`; a state store but no release store makes the offer 503
/// naming `--releases-dir`.
#[tokio::test]
async fn the_writes_say_which_half_is_missing() {
    let dir = Dir::new();
    let store =
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"));
    let no_rows = start(Some(store), None, &dir);
    let (code, body) = http(no_rows.api, "POST", "/v1/update/pause", true, None, b"").await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .expect("text")
            .contains("--state-store"),
        "{body}"
    );
    let no_store = start(None, Some(state()), &dir);
    let (code, body) = offer(&no_store, A).await;
    assert_eq!(code, 503);
    assert!(
        body["error"]
            .as_str()
            .expect("text")
            .contains("--releases-dir"),
        "{body}"
    );
}
