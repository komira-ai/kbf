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

#[path = "managed/admin_client.rs"]
mod admin_client;

#[path = "managed/admin_secret.rs"]
mod admin_secret;

#[path = "managed/proxy.rs"]
mod proxy;

#[path = "managed/two_children.rs"]
mod two_children;

#[path = "managed/roles.rs"]
mod roles;

#[path = "managed/lifecycle.rs"]
mod lifecycle;

#[path = "managed/writes.rs"]
mod writes;

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

async fn get_post(api: SocketAddr, path: &str) -> u16 {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Type: application/json\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\n\r\n"
    );
    exchange(api, &request).await.0
}
