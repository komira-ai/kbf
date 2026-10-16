//! The `kbf-gateway` binary: `envoy-config` prints the golden bootstraps; `serve` refuses
//! what it must, prints its start line, serves registrations and xDS, and stops cleanly
//! on SIGTERM.

mod support;

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use envoy_types::pb::envoy::config::endpoint::v3::ClusterLoadAssignment;
use envoy_types::pb::envoy::service::discovery::v3::DiscoveryRequest;
use envoy_types::pb::envoy::service::discovery::v3::aggregated_discovery_service_client::AggregatedDiscoveryServiceClient;
use futures::channel::mpsc as requests;
use kbf_proto::gateway::RegisterRequest;
use kbf_proto::gateway::registration_client::RegistrationClient;
use prost::Name as _;
use rcgen::ExtendedKeyUsagePurpose;
use support::Pki;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-gateway");
const BOUND: Duration = Duration::from_secs(20);

/// A running gateway, killed and reaped when dropped.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs `args` to exit: the exit code, stdout and stderr.
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("run kbf-gateway");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("UTF-8"),
        String::from_utf8(out.stderr).expect("UTF-8"),
    )
}

/// The farm's files for one test, in a directory of its own.
struct Files {
    dir: PathBuf,
}

impl Files {
    fn new(name: &str, pki: &Pki) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-gateway-{name}"));
        std::fs::create_dir_all(&dir).expect("create the directory");
        let identity = pki.leaf(&["localhost"], vec![ExtendedKeyUsagePurpose::ServerAuth]);
        std::fs::write(dir.join("ca.pem"), pki.ca_pem()).expect("write");
        std::fs::write(dir.join("gateway.pem"), identity.cert).expect("write");
        std::fs::write(dir.join("gateway.key"), identity.key).expect("write");
        Self { dir }
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).to_str().expect("UTF-8").to_owned()
    }

    /// The `serve` command line with `extra` flags after the files.
    fn serve(&self, extra: &[&str]) -> Command {
        self.serve_with_certificate("gateway.pem", extra)
    }

    /// The same, with the registration listener's certificate read from `cert`.
    fn serve_with_certificate(&self, cert: &str, extra: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(["serve", "--register-listen", "127.0.0.1:0"])
            .args(["--tls-cert", &self.path(cert)])
            .args(["--tls-key", &self.path("gateway.key")])
            .args(["--client-ca", &self.path("ca.pem")])
            .args(extra);
        command
    }
}

/// Catches: the printed bootstrap drifting from the golden files (the same bytes
/// `render` is tested against, through the flags a person types).
#[test]
fn envoy_config_prints_the_goldens() {
    let (code, plain, _) = run(&["envoy-config"]);
    assert_eq!(code, Some(0));
    assert_eq!(plain, include_str!("golden/envoy_plain.json"));
    let (code, tls, _) = run(&[
        "envoy-config",
        "--upstream-ca",
        "/etc/kbf/ca.pem",
        "--upstream-san-suffix",
        ".farm.example",
    ]);
    assert_eq!(code, Some(0));
    assert_eq!(tls, include_str!("golden/envoy_tls.json"));
}

/// Catches: upstream TLS asked for without the name check (or the check without the CA),
/// which would trust any certificate of the CA as any server.
#[test]
fn upstream_tls_needs_both_its_flags() {
    for flags in [
        ["--upstream-ca", "/etc/kbf/ca.pem"],
        ["--upstream-san-suffix", ".farm.example"],
    ] {
        let mut args = vec!["envoy-config"];
        args.extend(flags);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, Some(2), "{flags:?}: {stderr}");
        assert_eq!(stdout, "");
    }
}

/// Catches: unauthenticated xDS served off loopback; a missing TLS file starting a
/// gateway anyway.
#[test]
fn serve_refuses_a_public_xds_address_and_missing_files() {
    let files = Files::new("refusals", &Pki::new());
    let out = files
        .serve(&["--xds-listen", "192.0.2.1:18000"])
        .output()
        .expect("run");
    let stderr = String::from_utf8(out.stderr).expect("UTF-8");
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("not a loopback address"), "{stderr}");

    let out = files
        .serve_with_certificate("absent.pem", &[])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).expect("UTF-8");
    assert!(stderr.contains("absent.pem"), "{stderr}");
}

fn first_line(stdout: impl Read + Send + 'static) -> String {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let line = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find(|l| !l.is_empty());
        let _ = tx.send(line);
    });
    match rx.recv_timeout(BOUND) {
        Ok(Some(line)) => line,
        other => panic!("no start line within {BOUND:?}: {other:?}"),
    }
}

/// Catches: a start line that names the wrong addresses, a binary that does not wire the
/// registration service and ADS to the same registry, and one that does not stop
/// cleanly on SIGTERM.
#[tokio::test]
async fn serve_registers_servers_and_answers_envoy_until_sigterm() {
    let pki = Pki::new();
    let files = Files::new("serve", &pki);
    let mut child = Running(
        files
            .serve(&["--xds-listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kbf-gateway"),
    );
    let stdout = child.0.stdout.take().expect("stdout");
    let line = first_line(stdout);
    let words: Vec<&str> = line.split(' ').collect();
    assert_eq!(words.len(), 4, "{line}");
    assert_eq!(words[0], "kbf-gateway");
    let register = words[2].strip_prefix("register=").expect("register=");
    let xds = words[3].strip_prefix("xds=").expect("xds=");

    let server = pki.server("kbf1.farm.test");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca_pem()))
        .identity(Identity::from_pem(server.cert, server.key))
        .domain_name("localhost");
    let channel = Endpoint::from_shared(format!("https://{register}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect_lazy();
    RegistrationClient::new(channel)
        .register(RegisterRequest {
            reapi_address: "192.0.2.10:8980".to_owned(),
            is_leader: true,
            term: 4,
        })
        .await
        .expect("register with the binary");

    let channel = Endpoint::from_shared(format!("http://{xds}"))
        .expect("endpoint")
        .connect_lazy();
    let (send, received) = requests::unbounded();
    let mut stream = AggregatedDiscoveryServiceClient::new(channel)
        .stream_aggregated_resources(received)
        .await
        .expect("open the stream")
        .into_inner();
    send.unbounded_send(DiscoveryRequest {
        type_url: ClusterLoadAssignment::type_url(),
        ..DiscoveryRequest::default()
    })
    .expect("subscribe");
    let answer = stream
        .message()
        .await
        .expect("healthy")
        .expect("a response");
    assert_eq!(answer.version_info, "4:kbf1.farm.test@192.0.2.10:8980");

    // SAFETY: kill(2) on the child this test spawned and has not reaped.
    assert_eq!(
        unsafe { libc::kill(i32::try_from(child.0.id()).expect("pid"), libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + BOUND;
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(Instant::now() < deadline, "still running after SIGTERM");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(0));
}
