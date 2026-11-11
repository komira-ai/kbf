//! The real job service for kbf's tests: the `js-lease-server` of the jobscheduler release that
//! `kbf-js-version.toml` pins, started as a child process on loopback ports the system picks.
//!
//! **The program.** CI fetches the pinned archive first, with `cargo xtask fetch-pinned-js`, into
//! [`fetched_dir`]: that verb refuses an archive whose SHA-256 is not the pinned one or whose
//! build attestation is not the pinned one, and writes the archive it checked there. A test does
//! not run a program it finds lying about: [`program`] reads that archive again, refuses it
//! unless its SHA-256 is still the pin's, and unpacks the program from those same bytes into the
//! test's own directory. A missing archive, a missing row in the pin, and a changed byte are
//! each a failure of the test, never a skip.
//!
//! **The server.** [`RealJobServer::start`] runs the program with the job service on
//! (`--job-listen`), its lease port and its job port both on port 0 of `127.0.0.1`, a token file,
//! a pool that registering hosts join (which the job service needs), and a tick of an hour. It
//! waits for the line the program prints on standard output once every listener is bound and
//! serving (`listening on <addr>`), and takes the job port from the log line the program wrote
//! before it (`listening for the job service addr=<addr>`). Both are reads that end when the line
//! comes or the program exits: there is no sleep and no timer. It then registers one host with
//! an agent's hello and cordons it: the service admits a job only when a registered host could
//! ever serve it, a cordoned host counts, and it is given no work, so every job a test submits
//! stays queued until the test cancels it, whenever a tick runs. No agent runs. Dropping the
//! server kills and reaps the process; its directory goes after.
//!
//! [`RealJobServer::start_placing`] runs it for jobs that run: the same flags (the server's one
//! placement, js-sched, places the jobs) and the pool [`AGENT_POOL`], whose hosts
//! heartbeat four times a second, and no host of its own: the hosts are the agents a test
//! starts ([`agent`]), and a job is placed on the next heartbeat of one of them.

#![allow(
    dead_code,
    reason = "each test binary uses a part of this module, and its unit tests the rest"
)]

pub mod agent;
pub mod drop_answer;
pub mod fields;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use js_proto::v1::job_service_client::JobServiceClient;
use js_proto::v1::{Constraints, CordonHostRequest, JobFile, JobSpec, Needs, NetworkLevel};
use kbf_job_client::{Client, Config, Token};
use kbf_testkit::dir::TestDir;
use xtask::fetch::{self, Format, Unpack};
use xtask::js_pin::Pin;

/// The bearer token the server is started with.
pub const TOKEN: &str = "real-job-service-token-0123456789abcdef";

/// The pool the server's registering hosts join: what the job service admits jobs against.
pub const POOL: &str = r#"name = "kbf-tests"
class = "linux-host"
max_machines = 2

[shape]
cpu_millis = 8000
memory_bytes = 17179869184

[capabilities]
os = "linux"
arch = "x86_64"
"#;

/// The pool of [`RealJobServer::start_placing`]: the hosts are agents, which heartbeat every
/// 250 ms (so a queued job is placed soon after it is submitted) and are lost after 5 s of
/// silence; each is booked as two cores, 4 GiB of memory and 1 GiB of disk, which a hosted
/// runner offers (a job of the executor asks for half again its farm booking and 512 MiB of
/// memory, at most 3.5 GiB here, and for no disk). Its capabilities are what the scheduler holds
/// a host to when it admits a job: a job of the executor runs in a container (`exec`) with an
/// allow-listed network (`egress`), a job that asks for disk (none here does) needs `disk-gib`
/// (a whole GiB at least), and a host joins only if it reports them.
pub const AGENT_POOL: &str = r#"name = "kbf-tests"
class = "linux-host"
max_machines = 2
heartbeat_interval = "250ms"
grace = "5s"

[shape]
cpu_millis = 2000
memory_bytes = 4294967296
disk_bytes = 1073741824

[capabilities]
os = "linux"
arch = "x86_64"
exec = "container"
disk-gib = "1"
egress = "allow-list"
"#;

/// The pool's join token (at least 32 bytes), which the hello of [`HOST`] and of every agent
/// presents.
pub const JOIN: &str = "join-token-0123456789-abcdefghijklmnop";

/// The one host of the pool: registered and cordoned, so it admits jobs and runs none.
pub const HOST: &str = "kbf-test-host";

/// The image every job names: pinned by digest, as the service requires.
pub const IMAGE: &str = "registry.example/kbf-task@sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// The target the program is fetched for: this machine's, built against musl.
pub fn target() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

/// The pin at the root of the repository.
pub fn pin() -> Pin {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kbf-js-version.toml");
    Pin::read(&path).unwrap_or_else(|e| panic!("{e}"))
}

/// Where `cargo xtask fetch-pinned-js` puts what it fetched: `--dir target/tmp/js`, the
/// directory Cargo gives integration tests for their files, `js` under it.
pub fn fetched_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("js")
}

/// `program` of the release `pin` names, unpacked into `into` from the archive the fetch left
/// in `fetched`, after that archive's SHA-256 is checked against the pin again.
///
/// # Errors
/// The pin has no row for the program on this target; the archive is not in `fetched`; its
/// SHA-256 is not the pinned one; the program is not in it. Each message says what to do.
pub fn unpack_pinned(
    pin: &Pin,
    program: &str,
    fetched: &Path,
    into: &Path,
) -> Result<PathBuf, String> {
    unpack_pinned_for(pin, program, &target(), fetched, into)
}

/// [`unpack_pinned`] for `target`, which need not be this machine's.
///
/// # Errors
/// As [`unpack_pinned`], with the row looked up for `target`.
pub fn unpack_pinned_for(
    pin: &Pin,
    program: &str,
    target: &str,
    fetched: &Path,
    into: &Path,
) -> Result<PathBuf, String> {
    let archive = pin.archive(program, target).map_err(|e| e.to_string())?;
    let file = fetched.join(pin.file_name(archive));
    let bytes = std::fs::read(&file).map_err(|e| {
        format!(
            "{}: {e}; fetch it first: cargo xtask fetch-pinned-js --pin kbf-js-version.toml \
             --program {program} --target {target} --gh <gh> --dir {}",
            file.display(),
            fetched.display()
        )
    })?;
    let unpack = Unpack::new(Some(Format::TarGz), Some(pin.member(archive)))?;
    let out = into.join(program);
    fetch::install(&bytes, &archive.sha256, &unpack, &out)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(out)
}

/// The pinned `program`, unpacked into `into`; the test fails, naming why, when it cannot be.
pub fn program(name: &str, into: &Path) -> PathBuf {
    unpack_pinned(&pin(), name, &fetched_dir(), into).unwrap_or_else(|e| panic!("{e}"))
}

/// The job port the line `line` of the server's log names, when it is the line that says where
/// the job service listens.
pub fn job_addr_of(line: &str) -> Option<SocketAddr> {
    if !line.contains("listening for the job service") {
        return None;
    }
    line.split_whitespace()
        .find_map(|word| word.strip_prefix("addr="))
        .and_then(|addr| addr.parse().ok())
}

/// A `js-lease-server` with the job service on, until it drops.
pub struct RealJobServer {
    child: Reaped,
    lease_addr: SocketAddr,
    job_addr: SocketAddr,
    log: Arc<Mutex<String>>,
    /// Dropped last: the process is gone before its directory is removed.
    dir: TestDir,
}

/// A child process killed and reaped when it drops: from the moment it is spawned, so a
/// failed start leaves no server behind.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Writes `text` to `dir/name` with `mode`.
fn write(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
    path
}

impl RealJobServer {
    /// Starts the pinned server in a directory of its own named after `name`, and returns once
    /// it serves, with its one cordoned host registered.
    pub async fn start(name: &str) -> RealJobServer {
        let server = RealJobServer::launch(name, POOL).await;
        server.register_cordoned_host().await;
        server
    }

    /// Starts the pinned server for jobs that run: jobs are placed on the agents that join
    /// [`AGENT_POOL`]. It has no host until an agent registers.
    pub async fn start_placing(name: &str) -> RealJobServer {
        RealJobServer::launch(name, AGENT_POOL).await
    }

    async fn launch(name: &str, pool: &str) -> RealJobServer {
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
        let binary = program("js-lease-server", &dir);
        let token = write(&dir, "token", TOKEN, 0o600);
        let joins = write(&dir, "agent-tokens", &format!("{JOIN}\n"), 0o600);
        let pool = write(&dir, "pool.toml", pool, 0o644);
        let child = Command::new(&binary)
            .args(["--listen", "127.0.0.1:0", "--job-listen", "127.0.0.1:0"])
            .arg("--state-file")
            .arg(dir.join("leases.toml"))
            .args(["--boot", "initialise", "--tick", "1h"])
            .arg("--token-file")
            .arg(&token)
            .arg("--agent-token-file")
            .arg(&joins)
            .arg("--agent-pool-config")
            .arg(&pool)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("start {}: {e}", binary.display()));
        let mut child = Reaped(child);
        let stdout = child.0.stdout.take().expect("piped stdout");
        let stderr = child.0.stderr.take().expect("piped stderr");
        let log = Arc::new(Mutex::new(String::new()));
        let (found, job_port) = mpsc::channel();
        let kept = Arc::clone(&log);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(addr) = job_addr_of(&line) {
                    let _ = found.send(addr);
                }
                let mut log = kept.lock().unwrap_or_else(PoisonError::into_inner);
                log.push_str(&line);
                log.push('\n');
            }
        });
        let mut stdout = BufReader::new(stdout);
        let mut first = String::new();
        let _ = stdout.read_line(&mut first);
        // The rest of standard output is read and dropped, so a later line never meets a
        // closed pipe.
        std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
        let Some(lease_addr) = first
            .trim_end()
            .strip_prefix("listening on ")
            .and_then(|addr| addr.parse().ok())
        else {
            let _ = child.0.kill();
            let status = child.0.wait();
            let _ = reader.join();
            let log = log.lock().unwrap_or_else(PoisonError::into_inner);
            panic!(
                "js-lease-server did not start ({status:?}); it printed {first:?}; its log:\n{log}"
            );
        };
        // The job port is logged before the line above is printed (0.1.218 binds every
        // listener, logs each, then prints it); a release that stopped logging it would hold
        // this read until the job's timeout, and is a change the pin bump has to answer.
        let job_addr = job_port.recv().unwrap_or_else(|_| {
            let log = log.lock().unwrap_or_else(PoisonError::into_inner);
            panic!("js-lease-server named no job port; its log:\n{log}")
        });
        RealJobServer {
            child,
            lease_addr,
            job_addr,
            log,
            dir,
        }
    }

    /// Registers [`HOST`] in the pool, as an agent's hello does, and cordons it.
    ///
    /// The job service admits a job only when a registered host (or a provider's catalog)
    /// could ever serve it, and a cordoned host counts; a cordoned host takes no work, so every
    /// job stays queued whenever a tick runs. No agent runs: the host is a hello and nothing
    /// more.
    async fn register_cordoned_host(&self) {
        let lease_addr = self.lease_addr;
        let hello = serde_json::json!({
            "host_id": HOST,
            "pool": "kbf-tests",
            "agent_version": "0.1.0",
            "shape": {"cpu_millis": 8000, "memory_bytes": 17_179_869_184_u64, "gpus": 0},
            "capabilities": {"os": "linux", "arch": "x86_64"},
            "phase": "serving",
        });
        let answer = reqwest::Client::new()
            .post(format!("http://{lease_addr}/v1/agent/hello"))
            .header("authorization", format!("Bearer {JOIN}"))
            .header("content-type", "application/json")
            .body(hello.to_string())
            .send()
            .await
            .expect("the hello reaches the server");
        let status = answer.status();
        let body = answer.text().await.unwrap_or_default();
        assert_eq!(status, 200, "the hello: {body}\n{}", self.log());

        let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", self.job_addr))
            .expect("a loopback endpoint")
            .connect()
            .await
            .expect("connect to the job service");
        let mut request = tonic::Request::new(CordonHostRequest {
            host_id: HOST.to_owned(),
            cordon_secs: Some(3600),
        });
        let bearer = format!("Bearer {TOKEN}").parse().expect("ASCII");
        request.metadata_mut().insert("authorization", bearer);
        JobServiceClient::new(channel)
            .cordon_host(request)
            .await
            .unwrap_or_else(|status| panic!("cordon {HOST}: {status:?}\n{}", self.log()));
    }

    /// The address agents register at (the lease server's own listener).
    pub fn lease_addr(&self) -> SocketAddr {
        self.lease_addr
    }

    /// The hosts the job service lists, for a failure's message.
    pub async fn hosts(&self) -> String {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", self.job_addr))
            .expect("a loopback endpoint")
            .connect()
            .await;
        let Ok(channel) = channel else {
            return format!("the job service cannot be reached: {channel:?}");
        };
        let mut request = tonic::Request::new(js_proto::v1::ListHostsRequest::default());
        let bearer = format!("Bearer {TOKEN}").parse().expect("ASCII");
        request.metadata_mut().insert("authorization", bearer);
        match JobServiceClient::new(channel).list_hosts(request).await {
            Ok(hosts) => format!("{:#?}", hosts.into_inner()),
            Err(status) => format!("ListHosts: {status:?}"),
        }
    }

    /// The job service's address.
    pub fn job_addr(&self) -> SocketAddr {
        self.job_addr
    }

    /// What the server has logged so far.
    pub fn log(&self) -> String {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A client of the job service at `origin` (a server's, or a relay's in front of one) that
    /// presents `token`.
    pub fn client_at(origin: &str, token: &str) -> Client {
        let origin = origin.parse().expect("a loopback origin");
        let token = Token::new(token).expect("the token");
        Client::new(Config::new(origin, token)).expect("a plain-HTTP client")
    }

    /// A client of this server's job service.
    pub fn client(&self) -> Client {
        RealJobServer::client_at(&format!("http://{}", self.job_addr), TOKEN)
    }
}

/// A map of the pairs.
fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// A job both the real server and the fake admit, with every field the tests compare set: an
/// image by digest, a command and its arguments, environment, needs, constraints, a wall limit,
/// a file (whose contents no read returns), a priority and labels.
pub fn spec(id: &str) -> JobSpec {
    JobSpec {
        job_id: id.to_owned(),
        image: IMAGE.to_owned(),
        command: "/bin/build".to_owned(),
        args: vec!["--fast".to_owned(), "all".to_owned()],
        env: map(&[("RUST_LOG", "info"), ("TARGET", "x86_64")]),
        needs: Some(Needs {
            cpu_millis: 1000,
            memory_bytes: 1 << 30,
            ..Needs::default()
        }),
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            ..Constraints::default()
        }),
        wall_limit_seconds: Some(600),
        files: vec![JobFile {
            name: "input".to_owned(),
            contents: "presigned-url-not-for-reads".to_owned(),
        }],
        priority: 3,
        labels: map(&[("suite", "real-service")]),
        ..JobSpec::default()
    }
}

/// `spec` as a read answers it: every file's contents empty, and a network level left
/// unspecified recorded as `none`, the level the proto reads it as.
pub fn as_read(mut spec: JobSpec) -> JobSpec {
    for file in &mut spec.files {
        file.contents.clear();
    }
    if spec.network == i32::from(NetworkLevel::Unspecified) {
        spec.set_network(NetworkLevel::None);
    }
    spec
}
