//! Offers over the worker session (`docs/design/pull-updates.md` sections 5 and 6.4), end to
//! end: a real frontend over loopback with a release store and a state store, daemons that
//! speak the session, and the node's own update client (`kbf-update`) taking the offers. What
//! the server keeps of the statuses the nodes send is `tests/update_hosts.rs`.

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
use kbf_proto::update::UpdateOffer;
use kbf_proto::worker::{daemon_message, server_message};
use kbf_server::members::MembershipConfig;
use kbf_server::release::{Limits, ReleaseStore};
use kbf_server::token::ApiToken;
use kbf_server::{Api, EpochPolicy, Listeners, StateStore, bind_server_with_state};
use kbf_state::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::{FakeDaemon, HELLO_WAIT, INTERVAL, PROMPT, QUIET, hello, hello_on};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const A: &str = "0123456789abcdef0123456789abcdef01234567";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";

struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "kbf-server-update-offers-{}-{}",
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
        // The offers are looked at once a tick.
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

/// A running frontend, its directory, and the state store it was given.
struct Frontend {
    api: SocketAddr,
    worker: SocketAddr,
    state: Arc<hooked::Hooked>,
    dir: Dir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Frontend {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn start_with(releases: bool, state: bool) -> Frontend {
    let dir = Dir::new();
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let releases = releases.then(|| {
        Arc::new(ReleaseStore::open(&dir.0.join("releases"), Limits::default()).expect("open"))
    });
    let hooked = hooked::Hooked::new();
    let state = state.then(|| StateStore {
        store: hooked.clone() as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: MembershipConfig {
            ttl: Duration::from_secs(30),
            dwell: Duration::from_secs(120),
            replication: 1,
        },
        history: None,
    });
    let api = Api {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        token: Some(ApiToken::from_file(&path).expect("a usable token")),
        local_cache: None,
        releases,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    // Allows everything, and names `farm.getAuthorizer`, which turns `GET /v1/update/hosts` on.
    let policy = kbf_auth::Policy::from_json(
        r#"{"authenticationPolicy": {"allow": {}}, "farm": {"getAuthorizer": {"allow": {}}}}"#,
    )
    .expect("a policy");
    let bound = bind_server_with_state(
        Arc::new(Cache::<MemoryMetaLog, MemoryStore>::memory()),
        listeners(),
        Some(api),
        policy,
        state,
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
        state: hooked,
        dir,
        stop: Some(stop),
    }
}

fn start() -> Frontend {
    start_with(true, true)
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

impl Frontend {
    /// Stores one component of `commit` for `platform`: the tarball `bytes`, and an
    /// attestation when `attest`.
    async fn put(&self, commit: &str, platform: &str, name: &str, bytes: &[u8], attest: bool) {
        let path = format!("/v1/releases/{commit}/{platform}/{name}");
        let (code, _) = http(self.api, "PUT", &path, true, None, bytes).await;
        assert_eq!(code, 201, "{path}");
        if attest {
            self.attest(commit, platform, name).await;
        }
    }

    /// The attestation of a component already stored: its bytes are `bundle:<name>:<commit>`.
    async fn attest(&self, commit: &str, platform: &str, name: &str) {
        let path = format!("/v1/releases/{commit}/{platform}/{name}.attestation");
        let bundle = attestation(commit, name);
        let (code, _) = http(self.api, "PUT", &path, true, None, &bundle).await;
        assert_eq!(code, 201, "{path}");
    }

    /// Offers `commit` as the desired release; its generation.
    async fn desire(&self, commit: &str) -> u64 {
        let body = json!({ "commit": commit }).to_string();
        let (code, desired) = http(
            self.api,
            "POST",
            "/v1/update/desired",
            true,
            Some("application/json"),
            body.as_bytes(),
        )
        .await;
        assert_eq!(code, 200, "{desired}");
        desired["generation"].as_u64().expect("a generation")
    }

    /// The entry of `node` in `GET /v1/update/hosts` once `ready` accepts it.
    async fn host_until(&self, node: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + PROMPT;
        loop {
            let (code, hosts) = http(self.api, "GET", "/v1/update/hosts", false, None, b"").await;
            assert_eq!(code, 200, "{hosts}");
            let listed = hosts["hosts"]
                .as_array()
                .and_then(|all| all.iter().find(|h| h["node_id"] == node));
            if let Some(host) = listed.filter(|h| ready(h)) {
                return host.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no matching host {node}: {hosts}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// The tarball bytes of `name` at `commit`: different for each, so a mix-up shows.
fn tarball(commit: &str, name: &str) -> Vec<u8> {
    format!("tarball:{name}:{commit}").into_bytes()
}

fn attestation(commit: &str, name: &str) -> Vec<u8> {
    format!("bundle:{name}:{commit}").into_bytes()
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A complete linux-x86_64 release (`kbf`) of `commit`.
async fn linux(front: &Frontend, commit: &str) {
    front
        .put(commit, "linux-x86_64", "kbf", &tarball(commit, "kbf"), true)
        .await;
}

/// The next `UpdateOffer` the daemon is sent, within `within`.
async fn offer_within(d: &mut FakeDaemon, within: Duration) -> Option<UpdateOffer> {
    d.expect_within(within, |m| match m {
        server_message::Message::UpdateOffer(o) => Some(o.clone()),
        _ => None,
    })
    .await
}

async fn offer(d: &mut FakeDaemon) -> UpdateOffer {
    offer_within(d, PROMPT).await.expect("an UpdateOffer")
}

async fn linux_node(front: &Frontend, name: &str) -> FakeDaemon {
    FakeDaemon::connect(front.worker, hello(name, 4, 8))
        .await
        .expect("registered")
}

async fn mac_node(front: &Frontend, name: &str, arch: &str) -> FakeDaemon {
    let hello = hello_on(name, 4, 8, &[("arch", arch), ("os", "macos")]);
    FakeDaemon::connect(front.worker, hello)
        .await
        .expect("registered")
}

/// Catches an offer that is not sent after `Welcome`, one built from the wrong parts, and
/// one the node's own client cannot take. The Linux release also holds a `kbf-daemon`
/// tarball, which a Linux host must not be told about. The node is a real `kbf-update`
/// client: it fetches the offered tarball through `ReleaseService.Fetch` on the worker
/// listener into its inbox, and the status it then reports is what `/v1/update/hosts` and
/// `/v1/nodes` list.
///
/// Mutants seen red: no offer after `Welcome`; every stored component offered, not the
/// platform's; the digest, the size or the attestation taken from another component.
#[tokio::test]
async fn a_node_is_offered_the_desired_release_after_welcome_and_its_client_takes_it() {
    let front = start();
    linux(&front, A).await;
    front
        .put(A, "linux-x86_64", "kbf-daemon", b"not for linux", true)
        .await;
    assert_eq!(front.desire(A).await, 1);

    let mut node = linux_node(&front, "n1").await;
    let offered = offer(&mut node).await;
    let kbf = tarball(A, "kbf");
    assert_eq!(
        offered,
        UpdateOffer {
            generation: 1,
            commit: A.to_owned(),
            platform: "linux-x86_64".to_owned(),
            components: vec![kbf_proto::update::Component {
                name: "kbf".to_owned(),
                sha256: sha256(&kbf),
                size: kbf.len() as u64,
                attestation: attestation(A, "kbf"),
            }],
        }
    );

    let client_dir = front.dir.0.join("client");
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", front.worker))
        .expect("endpoint")
        .connect_lazy();
    let config = kbf_update::Config::new(
        client_dir.clone(),
        "linux-x86_64".to_owned(),
        "0123456789ab".to_owned(),
    );
    let (handle, worker) =
        kbf_update::start(config, Arc::new(kbf_update::ReleaseClient::new(channel)))
            .expect("the client starts");
    let running = tokio::spawn(worker.run());
    handle.offer(kbf_update::Offer::from(offered));
    let mut changes = handle.status();
    let reported = tokio::time::timeout(
        PROMPT,
        changes.wait_for(|s| s.offered_generation == 1 && s.state != kbf_update::State::Fetching),
    )
    .await
    .expect("the client settles in time")
    .expect("the client runs")
    .clone();
    let staged = client_dir
        .join("inbox")
        .join(kbf_update::inbox::dir_name(1, A))
        .join(format!("kbf-{A}-linux-x86_64.tar.gz"));
    assert_eq!(std::fs::read(staged).expect("the fetched tarball"), kbf);

    node.send(daemon_message::Message::UpdateStatus(
        kbf_proto::update::UpdateStatus::from(&reported),
    ));
    let host = front
        .host_until("n1", |h| h["offered_generation"] == 1)
        .await;
    assert_eq!(host["availability"], "ok");
    assert_eq!(host["running_commit"], "0123456789ab");
    assert_eq!(host["state"], "manual", "no helper reports on this host");
    assert_eq!(host["helper_present"], false);
    // It runs the prefix of the desired commit, as the open listing says too.
    assert_eq!(host["up_to_date"], true, "{host}");
    let (_, nodes) = http(front.api, "GET", "/v1/nodes", false, None, b"").await;
    assert_eq!(
        nodes["nodes"][0]["update"],
        json!({ "state": "manual", "up_to_date": true, "availability": "ok" })
    );
    running.abort();
}

/// Catches an offer sent once and never again, one a late node does not get, and a platform
/// whose completeness changes not noticed. Two Linux nodes hold generation 1; the desired
/// release moves to B and both are offered generation 2; a rollback to A is generation 3 (a
/// higher one); a node that connects after that gets the current offer. A Mac node, whose
/// platform is not complete for A, gets nothing while its tarball lacks an attestation, and
/// the offer (generation 1, the generation of A, repeated) when the attestation is stored.
///
/// Mutants seen red: the offer pushed once per session (no re-offer on a change); the table
/// not republished when only a platform's completeness changed.
#[tokio::test]
async fn offers_follow_the_desired_release_and_a_platforms_completeness() {
    let front = start();
    linux(&front, A).await;
    front.desire(A).await;
    let mut one = linux_node(&front, "n1").await;
    let mut two = linux_node(&front, "n2").await;
    let mut mac = mac_node(&front, "m1", "arm64").await;
    for node in [&mut one, &mut two] {
        assert_eq!(offer(node).await.generation, 1);
    }
    assert!(
        offer_within(&mut mac, QUIET).await.is_none(),
        "darwin has no release"
    );

    linux(&front, B).await;
    assert_eq!(front.desire(B).await, 2);
    for node in [&mut one, &mut two] {
        let offered = offer(node).await;
        assert_eq!((offered.generation, offered.commit.as_str()), (2, B));
    }
    assert_eq!(front.desire(A).await, 3, "a rollback outranks");
    for node in [&mut one, &mut two] {
        let offered = offer(node).await;
        assert_eq!((offered.generation, offered.commit.as_str()), (3, A));
    }
    let mut late = linux_node(&front, "n3").await;
    assert_eq!(offer(&mut late).await.generation, 3);
    assert!(
        offer_within(&mut one, QUIET).await.is_none(),
        "nothing repeats"
    );

    // The Mac's platform of A: a tarball first, then its attestation.
    front
        .put(
            A,
            "darwin-arm64",
            "kbf-daemon",
            &tarball(A, "kbf-daemon"),
            false,
        )
        .await;
    assert!(
        offer_within(&mut mac, QUIET).await.is_none(),
        "not attested yet"
    );
    front.attest(A, "darwin-arm64", "kbf-daemon").await;
    let offered = offer(&mut mac).await;
    assert_eq!(
        (offered.generation, offered.platform.as_str()),
        (3, "darwin-arm64")
    );
    assert_eq!(offered.components[0].name, "kbf-daemon");
    assert!(
        offer_within(&mut one, QUIET).await.is_none(),
        "Linux is told nothing of it"
    );
}

/// Catches a node offered a release that is not built for it or not complete: with A
/// complete for linux-x86_64 only, a Linux arm64 node, a Mac on x86_64 (no release is built
/// for it), a Mac on arm64 and a Linux host whose platform holds only the wrong component
/// (`kbf-daemon` for linux-arm64) are sent nothing, while the x86_64 Linux node, beside
/// them, is. A desired row that names no stored commit, or no commit at all, offers
/// nothing either.
///
/// Mutants seen red: a platform offered without the completeness check (the linux-arm64 node
/// gets an offer with no component); an unknown platform mapped to Linux.
#[tokio::test]
async fn an_unknown_or_incomplete_platform_is_offered_nothing() {
    let front = start();
    linux(&front, A).await;
    front
        .put(A, "linux-arm64", "kbf-daemon", b"the wrong component", true)
        .await;
    front.desire(A).await;
    let mut control = linux_node(&front, "control").await;
    assert_eq!(offer(&mut control).await.generation, 1);
    let arm = hello_on("arm", 4, 8, &[("arch", "arm64"), ("os", "linux")]);
    let mut nodes = vec![
        FakeDaemon::connect(front.worker, arm).await.expect("arm"),
        mac_node(&front, "intel-mac", "x86_64").await,
        mac_node(&front, "mac", "arm64").await,
    ];
    for node in &mut nodes {
        assert!(offer_within(node, QUIET).await.is_none());
    }

    // Rows the store cannot back: a commit nothing is stored for, and a name that is no commit.
    let rows = kbf_server::update::Rows::new(front.state.clone());
    for name in [B, "not-a-commit"] {
        rows.offer(
            &kbf_server::update::Release::new(name),
            kbf_types::FarmTime::from_millis(1),
        )
        .expect("the row");
        // The table follows the row within a tick; a node that connects first would be
        // sent the one before.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut fresh = linux_node(&front, &format!("fresh-{}", &name[..3])).await;
        assert!(offer_within(&mut fresh, QUIET).await.is_none(), "{name}");
    }
}

/// Catches a look that fails turning into offers that vanish, or into a table that is wrong:
/// while every read of the state store fails the node is sent nothing new and the table is
/// kept; once the store answers again the next change is offered.
#[tokio::test]
async fn a_look_that_fails_keeps_the_table() {
    let front = start();
    linux(&front, A).await;
    front.desire(A).await;
    let mut node = linux_node(&front, "n1").await;
    assert_eq!(offer(&mut node).await.generation, 1);
    linux(&front, B).await;
    front.state.fail_gets.store(1_000_000, Ordering::SeqCst);
    tokio::time::sleep(QUIET).await;
    // A node that connects during the outage is sent the table that was kept.
    let mut during = linux_node(&front, "n2").await;
    assert_eq!(offer(&mut during).await.generation, 1);
    front.state.fail_gets.store(0, Ordering::SeqCst);
    assert_eq!(front.desire(B).await, 2);
    assert_eq!(offer(&mut node).await.commit, B);
}

/// Catches offers sent by a server that has no release store or no state store (it has
/// nothing to build them from): such a frontend sends a node nothing.
#[tokio::test]
async fn a_frontend_without_a_release_store_or_a_state_store_offers_nothing() {
    for (releases, state) in [(false, true), (true, false)] {
        let front = start_with(releases, state);
        let mut node = linux_node(&front, "n1").await;
        assert!(offer_within(&mut node, QUIET).await.is_none());
    }
}
