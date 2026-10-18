//! A frontend run under the admin port (`kbf_server::managed`): a candidate that waits
//! for a takeover, `role`, `quiesce`, `resume`, `takeover` and `stand-down`, over one
//! in-memory state store that every process of a test shares (a SQLite file cannot be
//! opened twice, so these are the processes a proxy runs side by side on Postgres).

use kbf_proto::reapi::GetCapabilitiesRequest;
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_server::managed::{ADMIN_PROTOCOL, Managed, StoreSource};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{GENERATION, ManualClock};
use tonic::Code;

use support::done_within_quiet;
use tokio::task::JoinHandle;

use super::*;

/// Waits until `look` says yes, for at most twenty seconds.
async fn until(what: &str, mut look: impl AsyncFnMut() -> bool) {
    for _ in 0..1000 {
        if look().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never happened: {what}");
}

async fn readyz(api: SocketAddr) -> u16 {
    get(api, "/readyz").await.0
}

/// Gives a process that should not have acted the time it would have taken to.
async fn meanwhile() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// A store the processes share.
fn shared() -> Arc<Fake> {
    Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ))
}

fn managed_flags(node: &Node, more: &[&str]) -> FrontendFlags {
    let storage = format!("--storage=http://{}", node.addr);
    let mut argv = vec![
        storage.as_str(),
        "--admin-listen=127.0.0.1:0",
        "--heartbeat-interval-ms=100",
    ];
    if !more.iter().any(|m| m.starts_with("--state-store=")) {
        argv.push("--state-store=sqlite:///never-opened/state.db");
    }
    argv.extend_from_slice(more);
    flags(&argv).expect("flags")
}

/// A process serving in this test.
struct Child {
    admin: SocketAddr,
    reapi: SocketAddr,
    api: SocketAddr,
    id: String,
}

impl Child {
    async fn start(flags: &FrontendFlags, store: &Arc<Fake>) -> Self {
        let managed = Managed::start(
            flags,
            StoreSource::Shared(Arc::clone(store) as Arc<dyn Store>),
            std::future::pending::<()>(),
        )
        .await
        .expect("starts");
        let view = managed.view();
        let child = Self {
            admin: managed.admin(),
            reapi: view.reapi.expect("a REAPI listener"),
            api: view.api.expect("an operator API"),
            id: view.instance_id,
        };
        tokio::spawn(managed.serve());
        child
    }

    async fn call(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        exchange(self.admin, &request).await
    }

    async fn role(&self) -> Value {
        let (code, view) = self.call("GET", "/v1/admin/role", "").await;
        assert_eq!(code, 200, "{view}");
        view
    }

    async fn post(&self, verb: &str) -> (u16, Value) {
        self.call("POST", &format!("/v1/admin/{verb}"), "").await
    }

    async fn takeover(&self, expect: u64) -> (u16, Value) {
        let body = format!("{{\"expect\": {expect}}}");
        self.call("POST", "/v1/admin/takeover", &body).await
    }

    fn worker_of(view: &Value) -> Option<SocketAddr> {
        view["worker"]
            .as_str()
            .map(|a| a.parse().expect("an address"))
    }
}

/// One request and its answer, bounded: a process that never answers fails the test
/// instead of hanging it.
async fn exchange(addr: SocketAddr, request: &str) -> (u16, Value) {
    let exchanged = async {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("send");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        response
    };
    let response = tokio::time::timeout(Duration::from_secs(30), exchanged)
        .await
        .unwrap_or_else(|_| panic!("{addr} did not answer within 30 seconds"));
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok());
    (
        code.expect("a status code"),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// A scratch directory of the test's own, removed when dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("kbf-managed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }

    /// The `--api-token-file` flag for a token file of this directory, 0600.
    fn token_flag(&self) -> String {
        let file = self.0.join("token");
        std::fs::write(&file, format!("{TOKEN}\n")).expect("token file");
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("mode");
        format!("--api-token-file={}", file.display())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const TOKEN: &str = "a-token-that-is-long-enough-for-a-test";

fn generation(store: &Arc<Fake>) -> (u64, kbf_state::Version) {
    let row = store.get(GENERATION).expect("read").expect("a generation");
    let bytes = <[u8; 8]>::try_from(row.value.as_slice()).expect("eight bytes");
    (u64::from_be_bytes(bytes), row.version)
}

fn epoch(store: &Arc<Fake>) -> u64 {
    let row = store.get("meta/epoch").expect("read").expect("an epoch");
    u64::from_be_bytes(<[u8; 8]>::try_from(row.value.as_slice()).expect("eight bytes"))
}

/// A port nothing listens on.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

async fn refused(addr: SocketAddr) -> bool {
    TcpStream::connect(addr).await.is_err()
}

async fn capabilities(reapi: SocketAddr) -> Result<(), Code> {
    let channel = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("uri")
        .connect()
        .await
        .map_err(|_| Code::Unavailable)?;
    CapabilitiesClient::new(channel)
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .map(|_| ())
        .map_err(|status| status.code())
}

/// The code an Execute is refused with, however it is surfaced.
async fn execute_refused(client: &Client, job: &Job) -> Option<Code> {
    match client.execute_with(&job.action, false).await {
        Err(status) => Some(status.code()),
        Ok(mut ops) => match ops.message().await {
            Err(status) => Some(status.code()),
            Ok(_) => None,
        },
    }
}

/// Catches: a candidate that takes the generation when it opens the store (the holder
/// would be fenced by a process that serves nothing), that writes to the store at all
/// (the commit counter moves), that binds a worker listener (daemons would dial a process
/// that cannot schedule), or that runs the scheduler (an Execute is not refused). It
/// answers `/readyz`, the health service and `GetCapabilities`. Mutant: the candidate's
/// open takes the generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_takes_nothing_schedules_nothing_and_answers_readiness() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let (held, version) = generation(&store);
    let commits = store.commit_stats();

    let worker = format!("--worker-listen=127.0.0.1:{}", free_port());
    let flags = managed_flags(&node, &["--await-takeover", &worker]);
    let b = Child::start(&flags, &store).await;
    meanwhile().await;

    assert_eq!(
        generation(&store),
        (held, version),
        "the candidate took the generation"
    );
    assert_eq!(
        store.commit_stats(),
        commits,
        "the candidate wrote to the store"
    );
    let view = b.role().await;
    assert_eq!(view["role"], "candidate");
    assert_eq!(view["generation"], held);
    assert_eq!(view["held_generation"], Value::Null);
    assert!(Child::worker_of(&view).is_none());
    let configured = flags.server.worker_listen;
    assert!(
        refused(configured).await,
        "a candidate has a worker listener"
    );
    until("the candidate is ready", async || {
        readyz(b.api).await == 200
    })
    .await;
    capabilities(b.reapi).await.expect("GetCapabilities");
    let job = Job::new("a candidate does not schedule", &[]);
    let holder = Client::connect(a.reapi, flags.server.worker_listen).await;
    holder.upload(&job.blobs()).await;
    let client = Client::connect(b.reapi, configured).await;
    assert_eq!(
        execute_refused(&client, &job).await,
        Some(Code::Unimplemented),
        "a candidate serves Execute"
    );
    assert_eq!(a.role().await["role"], "active");
}

/// Catches: a `role` that does not say what the process is: a candidate that says active
/// (a proxy would route to a process that schedules nothing), a taken generation not
/// reported by the holder and the store, a quiesced process that says active, a stood
/// down one that says anything else, and an instance id that is not a UUID or does not
/// differ between processes. Mutant: `role` reports active for a candidate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_answers_truthfully_in_each_state() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;

    let view = a.role().await;
    assert_eq!(view["role"], "active");
    assert_eq!(view["admin_protocol"], ADMIN_PROTOCOL);
    assert_eq!(view["instance_id"], a.id);
    let (held, _) = generation(&store);
    assert_eq!(
        (
            view["generation"].as_u64(),
            view["held_generation"].as_u64()
        ),
        (Some(held), Some(held))
    );
    assert!(
        Child::worker_of(&view).is_some(),
        "an active process has a worker listener"
    );
    assert_ne!(a.id, b.id);
    for id in [&a.id, &b.id] {
        let parts: Vec<usize> = id.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12], "{id}");
        assert_eq!(id.as_bytes()[14], b'4', "{id}");
    }
    assert_eq!(b.role().await["role"], "candidate");

    assert_eq!(a.post("quiesce").await.1["role"], "quiesced");
    assert_eq!(a.role().await["role"], "quiesced");
    let (code, took) = b.takeover(held).await;
    assert_eq!(code, 200, "{took}");
    assert_eq!(took["role"], "active");
    assert_eq!(took["held_generation"], held + 1);
    assert_eq!(b.role().await["generation"], held + 1);
    assert_eq!(
        a.role().await["generation"],
        held + 1,
        "the old holder reports the store's generation"
    );
    assert_eq!(a.role().await["held_generation"], held);

    let (code, stood) = a.post("stand-down").await;
    assert_eq!(code, 200, "{stood}");
    assert_eq!(stood["role"], "stood-down");
    assert_eq!(stood["worker"], Value::Null);
    assert_eq!(a.role().await["role"], "stood-down");
    assert_eq!(b.role().await["role"], "active");
}

/// Catches: a takeover that loses a build in flight. A holds a daemon and a client's
/// Execute stream. After quiesce on A and a takeover on B: B holds the next generation and
/// the epoch is kept (the daemon, ended by A's stand-down, redials B and keeps the lease
/// it held), A's next write is `Fenced` (a cordon through its operator API leaves it
/// superseded), the client's stream, still on A, is answered with the result B took from
/// the daemon, and A refuses a resume (the store is not at its generation). Mutants: the
/// takeover bumps the epoch; the stand-down not ending the daemon's stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_after_a_quiesce_fences_the_old_holder_which_finishes_its_open_calls() {
    let node = Node::start().await;
    let store = shared();
    let scratch = Scratch::new("token");
    let token_flag = scratch.token_flag();
    let a = Child::start(&managed_flags(&node, &[&token_flag]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build across a takeover", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let (held, _) = generation(&store);
    let before = epoch(&store);

    assert_eq!(a.post("quiesce").await.0, 200);
    let (code, took) = b.takeover(held).await;
    assert_eq!(code, 200, "{took}");
    assert_eq!(
        generation(&store).0,
        held + 1,
        "B holds the next generation"
    );
    assert_eq!(epoch(&store), before, "a takeover bumped the epoch");

    // A's next write is refused: a cordon is a write through the fenced handle.
    let cordon = get_post(a.api, "/v1/nodes/node-a:cordon").await;
    assert_eq!(cordon, 200);
    until("A sees it is fenced", async || {
        a.role().await["superseded"] == true
    })
    .await;
    let (code, refused) = a.post("resume").await;
    assert_eq!(code, 409, "{refused}");
    assert_eq!(a.role().await["role"], "quiesced");

    assert_eq!(a.post("stand-down").await.0, 200);
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    let reached =
        Client::connect(b.reapi, Child::worker_of(&b.role().await).expect("worker")).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert_eq!(daemon.epoch, lease.term, "the epoch was kept");
    assert!(daemon.heartbeat(&[lease]).await);
    assert!(
        !done_within_quiet(&mut ops).await,
        "answered before the result"
    );
    let result = support::output(&reached, "built across a takeover", 0).await;
    assert!(
        daemon.report(ran(Some(lease), &result)).await.accepted,
        "B did not take the old lease"
    );
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
}

async fn get_post(api: SocketAddr, path: &str) -> u16 {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\n\r\n"
    );
    exchange(api, &request).await.0
}

/// Catches: a quiesce that does not stop scheduling (an Execute is accepted and the daemon
/// is handed work), one that cannot be undone (after a takeover that did not happen the
/// holder would never schedule again), and a resume that does not bring back what the
/// store holds. A quiesced A refuses an Execute and assigns nothing; after `resume` the
/// same action is queued and the daemon, connected the whole time, starts it. Mutants:
/// quiesce does nothing; resume does not clear the fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiesce_then_resume_restores_scheduling_from_the_store() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("work that waits for a resume", &[]);
    client.upload(&job.blobs()).await;
    let (held, _) = generation(&store);

    let (code, view) = a.post("quiesce").await;
    assert_eq!((code, view["role"].as_str()), (200, Some("quiesced")));
    assert_eq!(a.post("quiesce").await.0, 200, "quiesce is idempotent");
    assert!(
        execute_refused(&client, &job).await.is_some(),
        "a quiesced process queued work"
    );
    daemon.no_work().await;
    assert_eq!(generation(&store).0, held, "quiesce moved the generation");

    let (code, view) = a.post("resume").await;
    assert_eq!(
        (code, view["role"].as_str()),
        (200, Some("active")),
        "{view}"
    );
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let result = support::output(&client, "work that waits for a resume", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    assert_eq!(response(&done(&mut ops).await).result, Some(result));
}

/// Catches: a stand-down that leaves daemons attached (their streams are not ended), that
/// leaves the worker listener accepting (a daemon that redials reaches a process that
/// schedules nothing), that stays ready (a proxy would keep choosing it), or that cuts the
/// REAPI listener (open Execute and WaitExecution streams would end). A candidate or an
/// unknown call is refused. Mutant: the stand-down leaves the worker listener up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stand_down_ends_worker_streams_and_leaves_no_worker_listener() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    assert_eq!(readyz(a.api).await, 200);

    let (code, view) = a.post("stand-down").await;
    assert_eq!(
        (code, view["role"].as_str()),
        (200, Some("stood-down")),
        "{view}"
    );
    let ended = daemon.ended().await;
    assert_eq!(ended.code(), Code::Unavailable, "{ended}");
    until("the worker listener is gone", async || {
        refused(a_worker).await
    })
    .await;
    until("the process is not ready", async || {
        readyz(a.api).await == 503
    })
    .await;
    assert!(!refused(a.reapi).await, "the REAPI listener was cut");
    assert_eq!(
        a.post("stand-down").await.0,
        200,
        "stand-down is idempotent"
    );
    assert_eq!(a.post("quiesce").await.0, 409);
    assert_eq!(a.post("resume").await.0, 409);
}

/// Catches: an admin port reachable from off the host (the calls move the scheduler's role
/// and carry no credential): a non-loopback `--admin-listen` is refused at parse, and a
/// start given one anyway is refused, so is `--await-takeover` without the port, with a
/// handover socket, or a port without a state store. Loopback v4 and v6 are accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_port_refuses_a_non_loopback_address() {
    for bad in [
        "0.0.0.0:7001",
        "[::]:7001",
        "192.0.2.7:7001",
        "[2001:db8::1]:7001",
    ] {
        let refused = flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            &format!("--admin-listen={bad}"),
        ])
        .expect_err(bad);
        assert!(refused.to_string().contains("loopback"), "{bad}: {refused}");
    }
    for good in ["127.0.0.1:7001", "[::1]:7001"] {
        let parsed = flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            &format!("--admin-listen={good}"),
        ])
        .expect(good);
        assert_eq!(
            parsed.admin.expect("admin flags").listen,
            good.parse().unwrap()
        );
    }
    assert!(
        flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            "--await-takeover"
        ])
        .is_err()
    );
    assert!(
        flags(&["--storage=http://127.0.0.1:1", "--admin-listen=127.0.0.1:0"]).is_err(),
        "no state store"
    );
    assert!(
        flags(&[
            "--storage=http://127.0.0.1:1",
            "--state-store=sqlite:///x.db",
            "--admin-listen=127.0.0.1:0",
            "--await-takeover",
            "--handover-socket=/run/kbf/c.sock",
        ])
        .is_err()
    );

    let node = Node::start().await;
    let mut open = managed_flags(&node, &[]);
    open.admin.as_mut().expect("admin flags").listen = "0.0.0.0:0".parse().unwrap();
    let error = Managed::start(
        &open,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .err()
    .expect("a start on a public address is refused");
    assert!(error.to_string().contains("loopback"), "{error}");
}

/// Catches: an epoch that does not follow the rule. A takeover from a live holder keeps
/// it (daemons keep their leases), a candidate's open leaves it as it was, and a cold
/// start over the same store bumps it (a start with no live holder cannot trust what the
/// last one leased). Mutant: the takeover bumps the epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_epoch_is_kept_by_a_takeover_and_bumped_by_a_cold_start() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let first = epoch(&store);
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    assert_eq!(epoch(&store), first, "a candidate's open moved the epoch");
    let (held, _) = generation(&store);
    assert_eq!(a.post("quiesce").await.0, 200);
    assert_eq!(b.takeover(held).await.0, 200);
    assert_eq!(epoch(&store), first, "a takeover moved the epoch");
    let c = Child::start(&managed_flags(&node, &[]), &store).await;
    assert_eq!(epoch(&store), first + 1, "a cold start kept the epoch");
    assert_eq!(c.role().await["held_generation"], held + 2);
}

/// Catches: a takeover that writes when the holder moved on (the candidate would fence a
/// holder it was not told about), one that answers a body it does not understand, and
/// calls that change a role they do not apply to. A wrong `expect` leaves the generation
/// and the candidate as they were and names the generation found.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_that_do_not_apply_change_nothing() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let (held, version) = generation(&store);

    let (code, answer) = b.takeover(held + 5).await;
    assert_eq!(code, 409, "{answer}");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains(&held.to_string())),
        "{answer}"
    );
    assert_eq!(answer["role"], "candidate");
    assert_eq!(
        generation(&store),
        (held, version),
        "a refused takeover wrote"
    );
    assert_eq!(b.call("POST", "/v1/admin/takeover", "{}").await.0, 400);
    assert_eq!(
        b.call("POST", "/v1/admin/takeover", "{\"expect\": 1, \"more\": 2}")
            .await
            .0,
        400
    );
    for verb in ["quiesce", "resume", "stand-down"] {
        assert_eq!(b.post(verb).await.0, 409, "{verb} on a candidate");
    }
    assert_eq!(
        a.takeover(held).await.0,
        409,
        "takeover on an active process"
    );
    let (code, view) = a.post("resume").await;
    assert_eq!(
        (code, view["role"].as_str()),
        (200, Some("active")),
        "a resume of a process that never quiesced is a no-op"
    );
    assert_eq!(a.call("GET", "/v1/admin/nothing", "").await.0, 404);
    assert_eq!(a.role().await["role"], "active");
    assert_eq!(b.role().await["role"], "candidate");
}

/// Catches: an old holder that is fenced by a takeover and is never told to stand down
/// (the supervisor died, or the call was lost) leaving its callers hanging: a caller that
/// waits on Execute at A is answered once B writes the `done/` row, though A was only
/// found fenced by a refused write of its own (a cordon). Mutant: nothing follows the
/// `done/` rows of a process found superseded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fenced_holder_answers_its_waiting_callers_without_a_stand_down() {
    let node = Node::start().await;
    let store = shared();
    let scratch = Scratch::new("fenced");
    let token_flag = scratch.token_flag();
    let a = Child::start(&managed_flags(&node, &[&token_flag]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let a_worker = Child::worker_of(&a.role().await).expect("a worker listener");
    let client = Client::connect(a.reapi, a_worker).await;
    let mut daemon = client.daemon("node-a", 4, 8).await;
    let job = Job::new("a build whose holder is fenced", &[]);
    client.upload(&job.blobs()).await;
    let mut ops = client.execute(&job.action).await;
    let lease = daemon.start().await.lease_id.expect("a lease id");
    let (held, _) = generation(&store);

    // B takes the generation with no quiesce and no stand-down on A.
    assert_eq!(b.takeover(held).await.0, 200);
    assert_eq!(a.role().await["superseded"], false, "A has not written yet");
    assert_eq!(get_post(a.api, "/v1/nodes/node-a:cordon").await, 200);
    until("A finds it is fenced", async || {
        a.role().await["superseded"] == true
    })
    .await;
    assert_eq!(a.role().await["role"], "active", "no stand-down was made");
    // A found it is fenced and fenced its farm: a call that would queue is refused
    // UNAVAILABLE ("handing over"), not as the store's refusal of a write
    // (FAILED_PRECONDITION), which is what an unfenced farm that knows only its store is
    // superseded answers.
    let other = Job::new("work for a fenced holder", &[]);
    client.upload(&other.blobs()).await;
    until("the farm of a holder found fenced is fenced", async || {
        execute_refused(&client, &other).await == Some(Code::Unavailable)
    })
    .await;

    let b_worker = Child::worker_of(&b.role().await).expect("worker");
    let reached = Client::connect(b.reapi, b_worker).await;
    let mut daemon = reached.daemon("node-a", 4, 8).await;
    assert!(daemon.heartbeat(&[lease]).await);
    let result = support::output(&reached, "a build whose holder is fenced", 0).await;
    assert!(daemon.report(ran(Some(lease), &result)).await.accepted);
    let last = done(&mut ops).await;
    assert_eq!(response(&last).result, Some(result));
}

/// A process that can be told to stop, and whose end can be read.
struct Stoppable {
    child: Child,
    stop: oneshot::Sender<()>,
    ended: JoinHandle<Result<(), kbf_server::managed::ManagedError>>,
    line: String,
}

impl Stoppable {
    async fn start(flags: &FrontendFlags, source: StoreSource) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let managed = Managed::start(flags, source, async move {
            let _ = stopped.await;
        })
        .await
        .expect("starts")
        .escalating(std::future::pending::<()>())
        .notifying(None);
        let view = managed.view();
        let line = managed.start_line();
        let child = Child {
            admin: managed.admin(),
            reapi: view.reapi.expect("a REAPI listener"),
            api: view.api.expect("an operator API"),
            id: view.instance_id,
        };
        Self {
            child,
            stop,
            ended: tokio::spawn(managed.serve()),
            line,
        }
    }

    /// Stops it and returns how its serving ended.
    async fn stopped(self) -> Result<(), String> {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(20), self.ended)
            .await
            .expect("it ends")
            .expect("the task")
            .map_err(|error| error.to_string())
    }
}

/// Catches: a managed process that does not end when told to stop (a candidate waiting
/// for a takeover that never comes would hold its ports for ever; an active one would
/// ignore the signal), and a start line that does not name the admin port, the instance,
/// the role and the protocol a supervisor reads it for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_managed_process_ends_when_told_to_stop() {
    let node = Node::start().await;
    let store = shared();
    let source = || StoreSource::Shared(Arc::clone(&store) as Arc<dyn Store>);
    let active = Stoppable::start(&managed_flags(&node, &[]), source()).await;
    let candidate = Stoppable::start(&managed_flags(&node, &["--await-takeover"]), source()).await;
    for (process, role) in [(&active, "active"), (&candidate, "candidate")] {
        let line = &process.line;
        assert!(line.starts_with("kbf-frontend-admin admin="), "{line}");
        assert!(
            line.contains(&format!("admin={}", process.child.admin)),
            "{line}"
        );
        assert!(
            line.contains(&format!("instance={}", process.child.id)),
            "{line}"
        );
        assert!(
            line.contains(&format!("role={role} admin_protocol={ADMIN_PROTOCOL}")),
            "{line}"
        );
    }
    assert_eq!(candidate.stopped().await, Ok(()));
    assert_eq!(active.stopped().await, Ok(()));
}

/// Catches: a takeover that took the generation and could not start the frontend, which
/// the process hides (the supervisor would believe a candidate that holds the generation
/// and serves nothing): the call answers 500 with the reason and the process ends with it.
/// Here the worker address is taken by someone else.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_that_cannot_start_ends_the_process_with_the_reason() {
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let worker = format!("--worker-listen={}", taken.local_addr().expect("addr"));
    let b = Stoppable::start(
        &managed_flags(&node, &["--await-takeover", &worker]),
        StoreSource::Shared(Arc::clone(&store) as Arc<dyn Store>),
    )
    .await;
    let (held, _) = generation(&store);
    assert_eq!(a.post("quiesce").await.0, 200);
    let (code, answer) = b.child.takeover(held).await;
    assert_eq!(code, 500, "{answer}");
    assert!(
        answer["error"].as_str().is_some_and(|e| e.contains("bind")),
        "{answer}"
    );
    assert_eq!(generation(&store).0, held + 1, "the generation was taken");
    let ended = b
        .stopped()
        .await
        .expect_err("the process ends with the reason");
    assert!(ended.contains("bind"), "{ended}");
}

/// A store whose reads can be made to fail.
struct Flaky {
    inner: Arc<Fake>,
    failing: Arc<std::sync::atomic::AtomicBool>,
}

impl Flaky {
    fn check(&self) -> Result<(), StoreError> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Failed("planted".to_owned()));
        }
        Ok(())
    }
}

impl Store for Flaky {
    fn apply_batch(
        &self,
        batch: Vec<Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.check()?;
        self.inner.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.check()?;
        self.inner.scan_prefix(prefix)
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Catches: a resume that guesses when the store cannot be read (it must not put a
/// process back to work on a generation it cannot confirm: 500, still quiesced), and a
/// `role` that invents a generation (it says `null`); once the store answers again the
/// resume works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_cannot_be_read_is_reported_and_stops_a_resume() {
    let node = Node::start().await;
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flaky: Arc<dyn Store> = Arc::new(Flaky {
        inner: shared(),
        failing: Arc::clone(&failing),
    });
    let managed = Managed::start(
        &managed_flags(&node, &[]),
        StoreSource::Shared(flaky),
        std::future::pending::<()>(),
    )
    .await
    .expect("starts");
    let a = Child {
        admin: managed.admin(),
        reapi: managed.view().reapi.expect("reapi"),
        api: managed.view().api.expect("api"),
        id: managed.view().instance_id,
    };
    tokio::spawn(managed.serve());
    assert_eq!(a.post("quiesce").await.0, 200);
    failing.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(a.role().await["generation"], Value::Null);
    let (code, answer) = a.post("resume").await;
    assert_eq!(code, 500, "{answer}");
    assert_eq!(answer["role"], "quiesced");
    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(a.post("resume").await.0, 200);
}

/// Catches: a candidate that starts with an operator API address someone else holds
/// (the start must fail with the address, not run with half its listeners).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candidate_whose_listener_cannot_be_bound_does_not_start() {
    let node = Node::start().await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let api = format!("--api-listen={}", taken.local_addr().expect("addr"));
    let flags = managed_flags(&node, &["--await-takeover", &api]);
    let error = Managed::start(
        &flags,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .err()
    .expect("a bound address is refused");
    assert!(error.to_string().contains("bind"), "{error}");
}

/// Catches: a takeover that waits for ever on a probe's open health `Watch` stream (the
/// stand-in's graceful shutdown never finishes while it is open): the stand-in goes not
/// ready, the stream ends, and the takeover finishes at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_health_watch_does_not_hold_up_a_takeover() {
    use kbf_proto::grpc::health::v1::health_client::HealthClient;
    let node = Node::start().await;
    let store = shared();
    let a = Child::start(&managed_flags(&node, &[]), &store).await;
    let b = Child::start(&managed_flags(&node, &["--await-takeover"]), &store).await;
    let channel = Endpoint::from_shared(format!("http://{}", b.reapi))
        .expect("uri")
        .connect()
        .await
        .expect("connect");
    let mut watch = HealthClient::new(channel)
        .watch(HealthCheckRequest::default())
        .await
        .expect("watch")
        .into_inner();
    watch.message().await.expect("first").expect("a status");
    let (held, _) = generation(&store);
    assert_eq!(a.post("quiesce").await.0, 200);
    let started = std::time::Instant::now();
    assert_eq!(b.takeover(held).await.0, 200);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(_)) = watch.message().await {}
    })
    .await;
    assert!(end.is_ok(), "the watch on the stand-in was left open");
}

/// Catches: a process that cannot open the state store the flags name: a cold start over
/// a SQLite file takes its first generation; a candidate over another file opens it and
/// takes none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_managed_process_opens_the_store_its_flags_name() {
    let node = Node::start().await;
    let dir = std::env::temp_dir().join(format!("kbf-managed-flags-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let state = |name: &str| format!("--state-store=sqlite://{}/{name}.db", dir.display());
    let cold = Stoppable::start(&managed_flags(&node, &[&state("cold")]), StoreSource::Flags).await;
    let view = cold.child.role().await;
    assert_eq!(
        (view["role"].as_str(), view["held_generation"].as_u64()),
        (Some("active"), Some(1))
    );
    let waiting = Stoppable::start(
        &managed_flags(&node, &["--await-takeover", &state("waiting")]),
        StoreSource::Flags,
    )
    .await;
    let view = waiting.child.role().await;
    assert_eq!(
        (view["role"].as_str(), view["generation"].as_u64()),
        (Some("candidate"), Some(0))
    );
    assert_eq!(waiting.stopped().await, Ok(()));
    assert_eq!(cold.stopped().await, Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches: a candidate that needs an operator API (a REAPI-only deployment would not
/// start), a start from the flags with no state store that panics or starts over nothing
/// (it is refused naming the flag).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_candidate_serves_without_an_operator_api_and_the_flags_need_a_store() {
    let node = Node::start().await;
    let mut flags = managed_flags(&node, &["--await-takeover"]);
    flags.server.api_listen = None;
    let managed = Managed::start(
        &flags,
        StoreSource::Shared(shared()),
        std::future::pending::<()>(),
    )
    .await
    .expect("starts with no operator API");
    let view = managed.view();
    assert!(view.api.is_none());
    capabilities(view.reapi.expect("a REAPI listener"))
        .await
        .expect("GetCapabilities");

    let mut none = managed_flags(&node, &["--await-takeover"]);
    none.server.state_store = None;
    let error = Managed::start(&none, StoreSource::Flags, std::future::pending::<()>())
        .await
        .err()
        .expect("no store to open");
    assert!(error.to_string().contains("--state-store"), "{error}");
}
