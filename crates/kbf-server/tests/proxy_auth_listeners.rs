//! Who a call is behind a TLS-terminating proxy, over real listeners (`kbf_server::proxy_auth`,
//! issue #1095): an in-process server whose worker listener has no TLS (the proxy ended it)
//! and whose operator API has a token, and the `Membership` service behind the layer.

#![cfg(unix)]

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::channel::mpsc::unbounded;
use hooked::Hooked;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use kbf_server::identity::Presented;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL};
use kbf_server::members_rpc::MembershipService;
use kbf_server::proxy_auth::{Identity, NODE_ID, PROXY_AUTH, ProxyAuthLayer, ProxySecret, ROLE};
use kbf_server::token::ApiToken;
use kbf_server::{Api, Behind, DenyList, Listeners, bind_server_with_api};
use kbf_state::Store;
use kbf_storage::service::Admission;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Code, Status};

/// The 32 bytes of the test secret, and the header value that presents them.
const SECRET: [u8; 32] = [0x5a; 32];
const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const PROMPT: Duration = Duration::from_secs(5);

fn presented_secret() -> String {
    hex::encode(SECRET)
}

fn dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-proxy-auth");
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

/// A file holding `content` with `mode`, unique to this call.
fn file(content: &[u8], mode: u32) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = dir().join(format!("secret-{}-{n}", std::process::id()));
    std::fs::write(&path, content).expect("write the file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

fn secret() -> Arc<ProxySecret> {
    Arc::new(ProxySecret::from_file(&file(&SECRET, 0o600)).expect("a usable secret"))
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

// ---- the real listeners ------------------------------------------------------------

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

/// An in-process server behind a proxy: plain-text listeners on loopback, an operator API
/// with the token, no TLS flag anywhere.
struct Server_ {
    worker: SocketAddr,
    api: SocketAddr,
    _stop: tokio::sync::oneshot::Sender<()>,
}

fn start(with_proxy: bool) -> Server_ {
    start_denying(with_proxy, None)
}

fn start_denying(with_proxy: bool, deny_list: Option<DenyList>) -> Server_ {
    let loopback = addr("127.0.0.1:0");
    let listeners = Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        proxy: with_proxy.then(|| Behind {
            secret: secret(),
            deny_list,
        }),
        heartbeat_interval: Duration::from_millis(100),
        hello_wait: Duration::from_millis(300),
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
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let api = Api {
        listen: loopback,
        token: Some(token()),
        local_cache: None,
        releases: None,
    };
    let cache: Arc<MemoryCache> = Arc::new(Cache::memory());
    let bound = bind_server_with_api(cache, listeners, Some(api), async move {
        let _ = stopped.await;
    })
    .expect("bind");
    let (worker, api) = (bound.worker, bound.api.expect("an API address"));
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server_ {
        worker,
        api,
        _stop: stop,
    }
}

async fn channel(to: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{to}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

fn hello(node: &str) -> Hello {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    Hello {
        protocol_version: 1,
        node_id: node.to_owned(),
        capabilities: vec![
            entry("arch", "x86_64"),
            entry("cpus", "1"),
            entry("mem_gib", "1"),
            entry("drivers", "fake"),
        ],
        ..Hello::default()
    }
}

/// Sets the request header `name` to `value`.
fn put<T>(request: &mut tonic::Request<T>, name: &str, value: &str) {
    let key = tonic::metadata::MetadataKey::from_bytes(name.as_bytes()).expect("a key");
    let value: MetadataValue<_> = value.parse().expect("a value");
    request.metadata_mut().insert(key, value);
}

/// Opens a worker session whose `Hello` names `node`, with `claims` as request headers,
/// and waits for `Welcome` or the status the server refused it with.
async fn session(to: SocketAddr, node: &str, claims: &[(&str, &str)]) -> Result<(), Status> {
    let (tx, rx) = unbounded();
    tx.unbounded_send(DaemonMessage {
        message: Some(daemon_message::Message::Hello(hello(node))),
    })
    .expect("open");
    let mut request = tonic::Request::new(rx);
    for (name, value) in claims {
        put(&mut request, name, value);
    }
    let mut client = WorkerClient::new(channel(to).await);
    let mut inbound = tokio::time::timeout(PROMPT, client.session(request))
        .await
        .expect("an answer in time")?
        .into_inner();
    let first = tokio::time::timeout(PROMPT, inbound.message())
        .await
        .expect("an answer in time")?;
    match first {
        Some(ServerMessage {
            message: Some(server_message::Message::Welcome(_)),
        }) => Ok(()),
        other => panic!("expected Welcome or an error, got {other:?}"),
    }
}

/// The claims of a call the proxy forwarded: its secret, a node and a role.
fn forwarded(node: &str, role: &str) -> Vec<(String, String)> {
    vec![
        (PROXY_AUTH.to_owned(), presented_secret()),
        (NODE_ID.to_owned(), node.to_owned()),
        (ROLE.to_owned(), role.to_owned()),
    ]
}

async fn session_as(to: SocketAddr, node: &str, claims: &[(String, String)]) -> Result<(), Status> {
    let borrowed: Vec<(&str, &str)> = claims
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    session(to, node, &borrowed).await
}

/// Catches, over a real listener with no TLS (the proxy terminates it): the worker listener
/// believing identity headers that do not carry the secret (it must stay anonymous, and an
/// anonymous call is UNAUTHENTICATED, not a node), believing a wrong secret, believing the
/// role of a caller the proxy vouched for as a frontend, and a proxied daemon that is not
/// admitted at all.
///
/// Mutant seen red: the headers accepted without the secret check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_session_needs_the_proxys_word() {
    let server = start(true);
    let none: [(&str, &str); 0] = [];
    let anonymous = session(server.worker, "node-a", &none).await;
    assert_eq!(
        anonymous.expect_err("anonymous").code(),
        Code::Unauthenticated
    );

    let without_secret = [(NODE_ID, "node-a"), (ROLE, "daemon")];
    let refused = session(server.worker, "node-a", &without_secret).await;
    assert_eq!(
        refused.expect_err("no secret").code(),
        Code::Unauthenticated
    );

    let wrong = [
        (
            PROXY_AUTH,
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
        (NODE_ID, "node-a"),
        (ROLE, "daemon"),
    ];
    let refused = session(server.worker, "node-a", &wrong).await;
    assert_eq!(refused.expect_err("wrong").code(), Code::Unauthenticated);

    session_as(server.worker, "node-a", &forwarded("node-a", "daemon"))
        .await
        .expect("the proxy vouched for node-a");

    let frontend = session_as(server.worker, "node-a", &forwarded("node-a", "frontend")).await;
    assert_eq!(
        frontend.expect_err("frontend").code(),
        Code::PermissionDenied
    );

    let mut no_role = forwarded("node-a", "daemon");
    no_role.pop();
    let refused = session_as(server.worker, "node-a", &no_role).await;
    assert_eq!(refused.expect_err("no role").code(), Code::Unauthenticated);

    let mut no_node = forwarded("node-a", "daemon");
    no_node.remove(1);
    let refused = session_as(server.worker, "node-a", &no_node).await;
    assert_eq!(refused.expect_err("no node").code(), Code::Unauthenticated);
}

/// Catches: `Hello.node_id` not bound to the node the proxy named: a daemon vouched for as
/// `node-a` that says `node-b` in its `Hello` takes over another node's session.
///
/// Mutant seen red: the `Hello.node_id` binding skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_node_id_is_refused() {
    let server = start(true);
    let forged = session_as(server.worker, "node-b", &forwarded("node-a", "daemon")).await;
    let refused = forged.expect_err("forged");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("node-a"),
        "{}",
        refused.message()
    );
    session_as(server.worker, "node-b", &forwarded("node-b", "daemon"))
        .await
        .expect("node-b's own");
}

/// Catches: the deny list not applied to the node a proxy names (a denied node admitted),
/// or applied to every node.
///
/// Mutant seen red: `Behind::deny_list` dropped when the listener has no TLS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_node_is_refused_behind_the_proxy() {
    let list = DenyList::open(&file(b"node mac-07\n", 0o600)).expect("a deny list");
    let server = start_denying(true, Some(list));
    let denied = session_as(server.worker, "mac-07", &forwarded("mac-07", "daemon")).await;
    let refused = denied.expect_err("denied");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("mac-07"),
        "{}",
        refused.message()
    );
    session_as(server.worker, "mac-08", &forwarded("mac-08", "daemon"))
        .await
        .expect("not listed");
}

/// Catches: the blob calls of the worker listener not using the proxy's identity: a call
/// the proxy vouched for as a daemon with a node passes admission (the blob is simply
/// absent), one with no node is UNAUTHENTICATED, a storage role PERMISSION_DENIED, and one
/// without the secret UNAUTHENTICATED.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_calls_take_the_proxys_identity() {
    let server = start(true);
    let channel = channel(server.worker).await;
    let read = |claims: Vec<(String, String)>| {
        let channel = channel.clone();
        async move {
            let mut request = tonic::Request::new(ReadRequest {
                resource_name: format!("blobs/{}/0", "0".repeat(64)),
                read_offset: 0,
                read_limit: 0,
            });
            for (name, value) in &claims {
                put(&mut request, name, value);
            }
            let mut client = ByteStreamClient::new(channel);
            match client.read(request).await {
                Ok(response) => response.into_inner().message().await.map(drop),
                Err(status) => Err(status),
            }
        }
    };
    let passed = read(forwarded("node-a", "daemon")).await;
    assert!(
        passed.as_ref().is_ok() || passed.as_ref().is_err_and(|s| s.code() == Code::NotFound),
        "admitted, then answered for the blob: {passed:?}"
    );
    let mut no_node = forwarded("node-a", "daemon");
    no_node.remove(1);
    let refused = read(no_node).await.expect_err("no node");
    assert_eq!(refused.code(), Code::Unauthenticated);
    let storage = read(forwarded("node-a", "storage"))
        .await
        .expect_err("storage");
    assert_eq!(storage.code(), Code::PermissionDenied);
    let bare = read(vec![(NODE_ID.to_owned(), "node-a".to_owned())])
        .await
        .expect_err("no secret");
    assert_eq!(bare.code(), Code::Unauthenticated);
}

/// One HTTP/1.1 request to `api` with `headers`; the status code.
async fn http_status(api: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> u16 {
    let mut stream = TcpStream::connect(api).await.expect("connect");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {response:?}"))
}

/// Catches, over a real operator listener behind a proxy: a write that skips the token
/// because it came through the proxy (401 without it), a local caller that did not present
/// the secret writing with a good token (403), a wrong secret believed, and reads closed to
/// a local caller (they stay open, as documented). 404 for an unknown node is "past every
/// gate".
///
/// Mutant seen red: an operator write admitted without the token when proxied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_write_through_the_proxy_needs_the_token() {
    let server = start(true);
    let bearer = format!("Bearer {TOKEN}");
    let json = ("Content-Type", "application/json");
    let secret = presented_secret();
    let api = server.api;
    let write = |extra: Vec<(&'static str, &str)>| {
        let mut all = vec![json];
        all.extend(extra);
        let owned: Vec<(String, String)> = all
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        async move {
            let all: Vec<(&str, &str)> = owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            http_status(api, "POST", "/v1/nodes/ghost:cordon", &all).await
        }
    };
    assert_eq!(
        write(vec![(PROXY_AUTH, &secret)]).await,
        401,
        "the secret is not the token"
    );
    assert_eq!(
        write(vec![(PROXY_AUTH, &secret), ("Authorization", &bearer)]).await,
        404,
        "past every gate"
    );
    assert_eq!(
        write(vec![("Authorization", &bearer)]).await,
        403,
        "a local caller without the secret"
    );
    let wrong = hex::encode([0x5b; 32]);
    assert_eq!(
        write(vec![(PROXY_AUTH, &wrong), ("Authorization", &bearer)]).await,
        403,
        "a wrong secret"
    );
    assert_eq!(
        write(vec![
            (PROXY_AUTH, &secret),
            (ROLE, "admin"),
            ("Authorization", &bearer)
        ])
        .await,
        403,
        "a malformed identity"
    );
    assert_eq!(http_status(server.api, "GET", "/v1/nodes", &[]).await, 200);
    assert_eq!(http_status(server.api, "GET", "/healthz", &[]).await, 200);
}

/// Catches: the layer changing a server that is not behind a proxy: the headers do nothing
/// and the gate is loopback and token as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_flag_nothing_changes() {
    let server = start(false);
    let bearer = format!("Bearer {TOKEN}");
    let json = ("Content-Type", "application/json");
    let path = "/v1/nodes/ghost:cordon";
    let secret = presented_secret();
    assert_eq!(
        http_status(
            server.api,
            "POST",
            path,
            &[json, ("Authorization", &bearer)]
        )
        .await,
        404
    );
    assert_eq!(
        http_status(
            server.api,
            "POST",
            path,
            &[json, (PROXY_AUTH, &secret), (NODE_ID, "x")]
        )
        .await,
        401,
        "the headers are not a credential"
    );
    let anonymous = session(
        server.worker,
        "node-a",
        &[(NODE_ID, "node-b"), (ROLE, "daemon")],
    );
    anonymous.await.expect("plain text admits as before");
}

// ---- Membership ---------------------------------------------------------------------

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_millis(60),
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

async fn membership(secret: Arc<ProxySecret>) -> MembershipClient<Channel> {
    let members = Arc::new(Members::new(Hooked::new() as Arc<dyn Store>, config()));
    let incoming = tonic::transport::server::TcpIncoming::bind(addr("127.0.0.1:0")).expect("bind");
    let to = incoming.local_addr().expect("addr");
    let service = MembershipService::new(members, Admission::Certified).server();
    tokio::spawn(
        Server::builder()
            .layer(ProxyAuthLayer::new(Some(secret)))
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    MembershipClient::new(channel(to).await)
}

fn registration(instance: &str, at: &str) -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: instance.to_owned(),
        addr: at.to_owned(),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        catching_up: false,
    }
}

async fn register(
    client: &mut MembershipClient<Channel>,
    claims: &[(String, String)],
    instance: &str,
    at: &str,
) -> Result<(), Status> {
    let mut request = tonic::Request::new(registration(instance, at));
    for (name, value) in claims {
        put(&mut request, name, value);
    }
    client.register(request).await.map(drop)
}

/// Catches: Membership admitting a proxied caller by anything but the role and the one name
/// the proxy forwarded. The SAN check has no certificate behind the proxy, so the
/// conservative rule holds: the proxied node must be the host of both the instance id and
/// the address, an IP address is covered by nothing, and another node's host is refused;
/// the role must be `storage`, and without the secret the caller is anonymous.
///
/// Mutants seen red: the role not checked; the node name widened to any host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn membership_takes_the_proxys_storage_node_and_covers_by_its_name() {
    let mut client = membership(secret()).await;
    let storage = forwarded("storage-1", "storage");
    register(&mut client, &storage, "storage-1", "storage-1:8982")
        .await
        .expect("the proxied node is its own host");
    let ip = register(&mut client, &storage, "storage-1", "192.0.2.5:8982").await;
    let refused = ip.expect_err("an IP address has no name behind the proxy");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("do not cover"),
        "{}",
        refused.message()
    );
    let other = register(&mut client, &storage, "storage-2", "storage-1:8982").await;
    assert_eq!(
        other.expect_err("another instance").code(),
        Code::PermissionDenied
    );
    let other = register(&mut client, &storage, "storage-1", "storage-2:8982").await;
    assert_eq!(
        other.expect_err("another host").code(),
        Code::PermissionDenied
    );

    for role in ["daemon", "frontend"] {
        let refused = register(
            &mut client,
            &forwarded("storage-1", role),
            "storage-1",
            "storage-1:8982",
        )
        .await;
        assert_eq!(
            refused.expect_err(role).code(),
            Code::PermissionDenied,
            "{role}"
        );
    }
    let mut no_role = forwarded("storage-1", "storage");
    no_role.pop();
    let refused = register(&mut client, &no_role, "storage-1", "storage-1:8982").await;
    assert_eq!(refused.expect_err("no role").code(), Code::Unauthenticated);
    let mut no_node = forwarded("storage-1", "storage");
    no_node.remove(1);
    let refused = register(&mut client, &no_node, "storage-1", "storage-1:8982").await;
    assert_eq!(refused.expect_err("no node").code(), Code::Unauthenticated);
    let claims = [
        (NODE_ID.to_owned(), "storage-1".to_owned()),
        (ROLE.to_owned(), "storage".to_owned()),
    ];
    let bare = register(&mut client, &claims, "storage-1", "storage-1:8982").await;
    assert_eq!(bare.expect_err("no secret").code(), Code::Unauthenticated);

    // Without a proxy configured on a plain server, nothing is asked of the caller.
    let open = MembershipService::new(
        Arc::new(Members::new(Hooked::new() as Arc<dyn Store>, config())),
        Admission::Open,
    );
    let identity = Presented::Proxy(Identity::default());
    assert_eq!(open.admit_presented(&identity).expect("open"), None);
}

fn token() -> ApiToken {
    ApiToken::from_file(&file(format!("{TOKEN}\n").as_bytes(), 0o600)).expect("a token")
}

// ---- the binary ---------------------------------------------------------------------

/// The server binary with `extra` flags over a file store of its own, on any port.
fn binary(name: &str, extra: &[&str]) -> std::process::Command {
    let base = dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_kbf-server"));
    command
        .args(["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"])
        .arg("--cold-store")
        .arg(format!("file://{}", base.join("cold").display()))
        .arg("--meta-dir")
        .arg(base.join("meta"))
        .args(extra)
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY");
    command
}

/// Kills and reaps the child when dropped.
struct Running(std::process::Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The address after `key=` in the server's first output line.
fn address(line: &str, key: &str) -> SocketAddr {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
        .parse()
        .expect("an address")
}

/// Catches: `--proxy-auth-file` read at start but not given to the listeners: the worker
/// listener must take the proxy's identity with no TLS flag (a session is admitted only
/// with the secret), and the operator API must refuse a write without it, even with the
/// token. Over the real binary, so the flag, its file and the wiring are all in the path.
///
/// Mutant seen red: the secret read but the listeners built without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_serves_behind_the_proxy_when_given_the_flag() {
    use std::io::BufRead as _;
    let secret = file(&SECRET, 0o600);
    let token = file(format!("{TOKEN}\n").as_bytes(), 0o600);
    let mut command = binary(
        "behind",
        &[
            "--api-listen",
            "127.0.0.1:0",
            "--api-token-file",
            token.to_str().expect("UTF-8"),
            "--proxy-auth-file",
            secret.to_str().expect("UTF-8"),
        ],
    );
    let mut child = Running(
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let stdout = child.0.stdout.take().expect("stdout");
    let line = tokio::task::spawn_blocking(move || {
        std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find(|line| !line.is_empty())
            .expect("a start line")
    })
    .await
    .expect("read the start line");
    let (worker, api) = (address(&line, "worker="), address(&line, "api="));

    let anonymous = session(worker, "node-a", &[(NODE_ID, "node-a"), (ROLE, "daemon")]).await;
    assert_eq!(
        anonymous.expect_err("no secret").code(),
        Code::Unauthenticated
    );
    session_as(worker, "node-a", &forwarded("node-a", "daemon"))
        .await
        .expect("the proxy vouched for node-a");

    let bearer = format!("Bearer {TOKEN}");
    let json = ("Content-Type", "application/json");
    let path = "/v1/nodes/ghost:cordon";
    let local = http_status(api, "POST", path, &[json, ("Authorization", &bearer)]).await;
    assert_eq!(local, 403, "a local caller without the secret");
    let secret = presented_secret();
    let through = [json, (PROXY_AUTH, &secret), ("Authorization", &bearer)];
    assert_eq!(
        http_status(api, "POST", path, &through).await,
        404,
        "past every gate"
    );
}

/// Catches: a secret file others can read accepted at start (the server must exit 2 saying
/// which flag, which file and which mode, and promptly: one that starts instead is killed
/// and fails the test rather than hanging it).
///
/// Mutant seen red: `owner_only` accepting 0644.
#[test]
fn the_binary_refuses_a_readable_secret_file() {
    use std::io::Read as _;
    let open = file(&SECRET, 0o644);
    let open = open.to_str().expect("UTF-8");
    let mut command = binary("open", &["--proxy-auth-file", open]);
    let mut child = Running(
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn kbf-server"),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "kbf-server is still running 10 s after it should have refused to start"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child
        .0
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("UTF-8 stderr");
    assert_eq!(status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("--proxy-auth-file: "), "{stderr}");
    assert!(stderr.contains(open) && stderr.contains("0644"), "{stderr}");
}
