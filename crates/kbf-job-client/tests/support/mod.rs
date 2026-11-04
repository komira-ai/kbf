//! What the test files share: a fake with one host, jobs, the fully populated messages the
//! goldens encode, and the PKI of the TLS tests.
#![allow(
    dead_code,
    reason = "each test file is its own crate and uses a part of this module"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{
    Constraints, ExecMode, Job, JobFile, JobResult, JobSpec, JobState, JobTimes, Needs,
    NetworkLevel, PortForward, ResourceUsage, ResultReason, Shape, StartFailureCode,
};
use kbf_job_client::fake::{FakeJobServer, Machine};
use kbf_job_client::{Client, Config, Error};
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub const IMAGE: &str = "registry.example/kbf-task@sha256:0000000000000000000000000000000000000000000000000000000000000000";
pub const GIB: u64 = 1 << 30;
/// The time the full samples count from, in milliseconds since the Unix epoch.
pub const T0: u64 = 1_800_000_000_000;
/// The host every fake of [`fake`] has.
pub const HOST: &str = "host-a";

/// A Linux host of 16 cores, 64 GiB, 100 GiB of disk and one GPU of the class the full
/// sample asks for, running jobs in containers.
pub fn machine() -> Machine {
    Machine {
        shape: Shape {
            cpu_millis: 16_000,
            memory_bytes: 64 * GIB,
            gpus: 1,
            gpu_class: "nvidia-a100-sxm4-80gb".to_owned(),
            disk_bytes: Some(100 * GIB),
        },
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
    }
}

/// A fake with the host [`HOST`] of [`machine`], and a client of it.
pub async fn fake() -> (FakeJobServer, Client) {
    let server = FakeJobServer::start().await.unwrap();
    server.add_host(HOST, machine());
    let client = server.client().unwrap();
    (server, client)
}

/// A client of `server` whose calls give up after `request`.
pub fn client_with_deadline(server: &FakeJobServer, request: Duration) -> Client {
    let config = server.config().unwrap();
    Client::new(config.with_timeouts(Config::CONNECT_TIMEOUT, request)).unwrap()
}

/// The smallest job the fake takes: one core, 1 GiB, ten minutes, on Linux.
pub fn spec(id: &str) -> JobSpec {
    JobSpec {
        job_id: id.to_owned(),
        image: IMAGE.to_owned(),
        command: "/bin/build".to_owned(),
        needs: Some(Needs {
            cpu_millis: 1000,
            memory_bytes: GIB,
            ..Needs::default()
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            ..Constraints::default()
        }),
        wall_limit_seconds: Some(600),
        ..JobSpec::default()
    }
}

/// A map of the pairs, inserted in the order given.
pub fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// The fully populated job spec of `js-proto`'s tests (and so of the goldens), its one file
/// holding `contents`.
pub fn full_spec(contents: &str) -> JobSpec {
    JobSpec {
        job_id: "job-1".to_owned(),
        image: format!(
            "registry.example/app@sha256:{}",
            "0123456789abcdef".repeat(4)
        ),
        command: "/bin/run".to_owned(),
        args: vec!["--fast".to_owned(), "input".to_owned()],
        env: map(&[("B_VAR", "2"), ("A_VAR", "1")]),
        needs: Some(Needs {
            cpu_millis: 1500,
            memory_bytes: 2 << 30,
            disk_bytes: Some(1 << 30),
            gpus: 1,
            gpu_class: "nvidia-a100-sxm4-80gb".to_owned(),
            whole_host: true,
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            exec: ExecMode::Container.into(),
        }),
        wall_limit_seconds: Some(3600),
        network: NetworkLevel::AllowList.into(),
        ports: vec![PortForward {
            host_port: 8080,
            container_port: 80,
        }],
        files: vec![JobFile {
            name: "upload-url".to_owned(),
            contents: contents.to_owned(),
        }],
        priority: 7,
        labels: map(&[("team", "build"), ("kind", "test")]),
    }
}

/// The fully populated job of `js-proto`'s tests, as a read returns it.
pub fn full_job() -> Job {
    Job {
        id: "job-1".to_owned(),
        spec: Some(full_spec("")),
        state: JobState::FailedToStart.into(),
        start_failure: StartFailureCode::AgentRefused.into(),
        start_failure_detail: "image-fetch-failed".to_owned(),
        host_id: "host-a".to_owned(),
        times: Some(times()),
        host_silent_since_ms: Some(T0 + 4000),
        cancel_requested: true,
    }
}

fn times() -> JobTimes {
    JobTimes {
        queued_at_ms: T0,
        placed_at_ms: Some(T0 + 1000),
        started_at_ms: Some(T0 + 2000),
        ended_at_ms: Some(T0 + 3000),
    }
}

/// The fully populated result `tests/golden/job_result.hex` encodes.
pub fn full_result() -> JobResult {
    JobResult {
        state: JobState::Failed.into(),
        exit_code: Some(3),
        reason: ResultReason::Exit.into(),
        stdout_tail: b"built 12 targets\n".to_vec(),
        stderr_tail: b"error: link failed\n".to_vec(),
        truncated: true,
        times: Some(times()),
        usage: Some(ResourceUsage {
            cpu_usec: 1_500_000,
            peak_memory_bytes: 2 << 30,
            disk_bytes: Some(1 << 30),
            gpus: 1,
        }),
    }
}

/// The text of a golden file.
pub fn golden_text(name: &str) -> String {
    let path = format!("{}/tests/golden/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The bytes of a golden `.hex` file: hex pairs separated by white space, each line's text
/// from `#` on a comment.
pub fn golden_bytes(name: &str) -> Vec<u8> {
    golden_text(name)
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default())
        .flat_map(str::split_whitespace)
        .map(|pair| {
            assert_eq!(pair.len(), 2, "{pair:?} is not one byte in hex");
            u8::from_str_radix(pair, 16).expect("a hex pair")
        })
        .collect()
}

/// The status code of a definite answer; panics on any other error.
pub fn status_code(error: &Error) -> tonic::Code {
    match error {
        Error::Status { code, .. } => *code,
        other => panic!("not a definite status: {other:?}"),
    }
}

/// Moves the job `id` of `server` from queued to running on [`HOST`].
pub fn to_running(server: &FakeJobServer, id: &str) {
    assert!(server.place(id, HOST), "place {id}");
    assert!(server.starting(id), "start {id}");
    assert!(server.run(id), "run {id}");
}

/// A CA and a certificate for `localhost` under it.
pub struct Pki {
    pub ca: String,
    chain: String,
    key: String,
}

pub fn pki() -> Pki {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    Pki {
        ca: ca.pem(),
        chain: leaf.pem(),
        key: key.serialize_pem(),
    }
}

/// The server side of `pki`: its certificate for `localhost` and its key.
pub fn server_config(pki: &Pki) -> Arc<rustls::ServerConfig> {
    let chain: Vec<_> = CertificateDer::pem_slice_iter(pki.chain.as_bytes())
        .map(Result::unwrap)
        .collect();
    let key = PrivateKeyDer::from_pem_slice(pki.key.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    Arc::new(config)
}
