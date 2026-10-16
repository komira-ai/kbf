//! A real Envoy, the gateway and two stand-in servers: the pinned Envoy release binary
//! runs the generated bootstrap, learns the leader over ADS, and routes REAPI calls to it;
//! then the leader claim moves and Envoy follows, and a claim of a server that fails its
//! health check is not routed to.
//!
//! Ignored in the plain test run: the binary is not in the repository. CI's test-x86 job
//! fetches the pinned release (`cargo xtask fetch`) and runs these with `--ignored`,
//! naming the binary in `KBF_TEST_ENVOY`. Run without it, each test fails rather than
//! passes.
//!
//! Waiting is on Envoy's admin `/clusters` listing, polled until the condition holds or a
//! bound runs out; nothing is assumed from elapsed time.

mod support;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::Stream;
use kbf_gateway::envoy::{Options, render};
use kbf_proto::gateway::RegisterRequest;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_server::{Health, HealthServer};
use kbf_proto::grpc::health::v1::{
    HealthCheckRequest, HealthCheckResponse, HealthListRequest, HealthListResponse,
};
use kbf_proto::reapi::capabilities_client::CapabilitiesClient;
use kbf_proto::reapi::capabilities_server::{Capabilities, CapabilitiesServer};
use kbf_proto::reapi::{GetCapabilitiesRequest, ServerCapabilities};
use support::{Pki, Running, start};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tonic::transport::Server;
use tonic::{Code, Request, Response, Status};

/// The pinned Envoy binary CI fetched.
fn envoy_binary() -> PathBuf {
    let path = std::env::var_os("KBF_TEST_ENVOY")
        .expect("KBF_TEST_ENVOY names the pinned Envoy binary (CI's test-x86 job fetches it)");
    PathBuf::from(path)
}

/// How long a wait for Envoy to reach a state may take before the test gives up.
const BOUND: Duration = Duration::from_secs(60);

/// A free loopback port: bound and released, so another process could race for it, which
/// on a test machine it does not.
fn free_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address")
}

/// A stand-in server: REAPI `GetCapabilities` that names the server (its id is the
/// `low_api_version` minor), and a health service whose answer the test switches.
struct Stand {
    id: i32,
    serving: Arc<AtomicBool>,
}

#[tonic::async_trait]
impl Capabilities for Stand {
    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        Ok(Response::new(ServerCapabilities {
            low_api_version: Some(kbf_proto::build::bazel::semver::SemVer {
                major: 2,
                minor: self.id,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

type Watch = Pin<Box<dyn Stream<Item = Result<HealthCheckResponse, Status>> + Send>>;

#[tonic::async_trait]
impl Health for Stand {
    async fn check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let status = if self.serving.load(Ordering::SeqCst) {
            ServingStatus::Serving
        } else {
            ServingStatus::NotServing
        };
        Ok(Response::new(HealthCheckResponse {
            status: status.into(),
        }))
    }

    async fn list(
        &self,
        _request: Request<HealthListRequest>,
    ) -> Result<Response<HealthListResponse>, Status> {
        Err(Status::unimplemented("list"))
    }

    type WatchStream = Watch;

    async fn watch(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<Watch>, Status> {
        Err(Status::unimplemented("watch"))
    }
}

/// A running stand-in server.
struct StandIn {
    address: SocketAddr,
    serving: Arc<AtomicBool>,
    name: String,
}

fn stand_in(id: i32) -> StandIn {
    let address = free_port();
    let serving = Arc::new(AtomicBool::new(true));
    let stand = || Stand {
        id,
        serving: Arc::clone(&serving),
    };
    let server = Server::builder()
        .add_service(CapabilitiesServer::new(stand()))
        .add_service(HealthServer::new(stand()))
        .serve(address);
    tokio::spawn(server);
    StandIn {
        address,
        serving,
        name: format!("kbf{id}.farm.test"),
    }
}

/// Envoy, killed and reaped when dropped.
struct EnvoyProcess(Child);

impl Drop for EnvoyProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Front {
    _envoy: EnvoyProcess,
    reapi: SocketAddr,
    admin: SocketAddr,
    /// Where Envoy's standard error goes, for the failure message of a wait.
    log: PathBuf,
}

/// Runs Envoy on the generated bootstrap for `gateway`.
fn run_envoy(dir: &Path, gateway: &Running) -> Front {
    let (reapi, admin) = (free_port(), free_port());
    let text = render(&Options {
        listen: reapi,
        xds: gateway.xds,
        admin,
        upstream: None,
    });
    std::fs::create_dir_all(dir).expect("directory");
    let config = dir.join("envoy.json");
    std::fs::write(&config, text).expect("write the bootstrap");
    let log = dir.join("envoy.log");
    let stderr = std::fs::File::create(&log).expect("Envoy's log file");
    let child = Command::new(envoy_binary())
        .args([
            "--disable-hot-restart",
            "--concurrency",
            "1",
            "--log-level",
            "warn",
        ])
        .arg("--config-path")
        .arg(&config)
        .stdin(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("start Envoy");
    Front {
        _envoy: EnvoyProcess(child),
        reapi,
        admin,
        log,
    }
}

/// Envoy's `/clusters` text.
async fn clusters(admin: SocketAddr) -> Option<String> {
    let mut stream = tokio::net::TcpStream::connect(admin).await.ok()?;
    stream
        .write_all(b"GET /clusters HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).await.ok()?;
    Some(text)
}

/// Waits until `/clusters` satisfies `condition`; fails the test, with the last listing,
/// when `BOUND` runs out.
async fn until(front: &Front, what: &str, condition: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + BOUND;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Some(text) = clusters(front.admin).await {
            if condition(&text) {
                return;
            }
            last = text;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let log = std::fs::read_to_string(&front.log).unwrap_or_default();
    let tail: String = log
        .chars()
        .rev()
        .take(6000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    panic!("{what}: not reached within {BOUND:?}; last /clusters:\n{last}\nEnvoy's log:\n{tail}");
}

/// The endpoint `address` of the leader cluster is healthy in `listing`.
fn healthy(listing: &str, address: SocketAddr) -> bool {
    listing.lines().any(|line| {
        line.starts_with(&format!("kbf_leader::{address}::health_flags::"))
            && line.ends_with("healthy")
    })
}

/// The leader cluster lists `address` at all.
fn lists(listing: &str, address: SocketAddr) -> bool {
    listing
        .lines()
        .any(|line| line.starts_with(&format!("kbf_leader::{address}::")))
}

fn claim(address: SocketAddr, is_leader: bool, term: u64) -> RegisterRequest {
    RegisterRequest {
        reapi_address: address.to_string(),
        is_leader,
        term,
    }
}

/// Which stand-in answers a `GetCapabilities` sent through Envoy.
async fn answered_by(reapi: SocketAddr) -> Result<i32, Status> {
    let mut client = CapabilitiesClient::connect(format!("http://{reapi}"))
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let capabilities = client
        .get_capabilities(GetCapabilitiesRequest::default())
        .await?
        .into_inner();
    Ok(capabilities.low_api_version.expect("a version").minor)
}

/// Catches: Envoy not learning the leader over ADS from the generated bootstrap (a field
/// the gateway's resource or the config gets wrong shows as an endpoint that never
/// appears); calls not reaching the leader; the leader claim moving without Envoy
/// following it; and a claim whose server fails its health check being routed to (the
/// stale-claim guard).
#[tokio::test]
#[ignore = "needs the pinned Envoy release (KBF_TEST_ENVOY); CI's test-x86 job runs it"]
async fn envoy_routes_to_the_current_leader() {
    let pki = Pki::new();
    let gateway = start(&pki);
    let (one, two) = (stand_in(1), stand_in(2));
    let mut first = gateway.registrar(&pki.server(&one.name));
    let mut second = gateway.registrar(&pki.server(&two.name));
    first
        .register(claim(one.address, true, 1))
        .await
        .expect("one");
    second
        .register(claim(two.address, false, 1))
        .await
        .expect("two");
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-gateway-envoy");
    let front = run_envoy(&dir, &gateway);

    until(&front, "the first leader healthy", |l| {
        healthy(l, one.address)
    })
    .await;
    assert_eq!(answered_by(front.reapi).await.expect("through Envoy"), 1);

    // The claim moves: the second server leads at a higher term, the first steps down.
    second
        .register(claim(two.address, true, 2))
        .await
        .expect("two leads");
    first
        .register(claim(one.address, false, 1))
        .await
        .expect("one follows");
    until(&front, "the second leader healthy", |l| {
        healthy(l, two.address) && !lists(l, one.address)
    })
    .await;
    assert_eq!(answered_by(front.reapi).await.expect("through Envoy"), 2);

    // A claim that is stale: the gateway still names the second server, which now fails
    // its health check. Envoy must not send to it, and must not panic-route either.
    two.serving.store(false, Ordering::SeqCst);
    until(&front, "the stale claim unhealthy", |l| {
        lists(l, two.address) && !healthy(l, two.address)
    })
    .await;
    let refused = answered_by(front.reapi).await.expect_err("nobody serves");
    assert!(
        matches!(
            refused.code(),
            Code::Unavailable | Code::Unknown | Code::Internal
        ),
        "{refused}"
    );
}

/// Catches: Envoy refusing the generated bootstrap, with or without upstream TLS (a field
/// it does not know, a wrong type), which `--mode validate` reports without binding a
/// port. The TLS variant names a CA file made for the test.
#[test]
#[ignore = "needs the pinned Envoy release (KBF_TEST_ENVOY); CI's test-x86 job runs it"]
fn envoy_validates_the_generated_bootstrap() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-gateway-validate");
    std::fs::create_dir_all(&dir).expect("directory");
    let ca = dir.join("ca.pem");
    std::fs::write(&ca, Pki::new().ca_pem()).expect("write the CA");
    let options = |upstream| Options {
        listen: "127.0.0.1:8980".parse().expect("address"),
        xds: "127.0.0.1:18000".parse().expect("address"),
        admin: "127.0.0.1:9901".parse().expect("address"),
        upstream,
    };
    let tls = kbf_gateway::envoy::Upstream {
        ca,
        san_suffix: ".farm.example".to_owned(),
    };
    for (name, options) in [("plain", options(None)), ("tls", options(Some(tls)))] {
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, render(&options)).expect("write");
        let out = Command::new(envoy_binary())
            .args([
                "--mode",
                "validate",
                "--disable-hot-restart",
                "--log-level",
                "warn",
            ])
            .arg("--config-path")
            .arg(&path)
            .output()
            .expect("run Envoy");
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
