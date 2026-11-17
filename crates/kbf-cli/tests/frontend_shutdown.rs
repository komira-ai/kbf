//! Stopping a `kbf frontend` process while an action runs as a job of the job service (issue
//! #168, and kbf#791): SIGTERM keeps an open Execute or WaitExecution stream until its job is
//! done, because Buck2 does not recover a stream that breaks; when `--shutdown-timeout-secs`
//! runs out the streams end UNAVAILABLE; a second SIGTERM ends the drain; and the drain
//! endpoint drains the process the same way. The job service is in this process
//! (`kbf_job_client::fake`), the frontend and the storage node it serves from are children, and
//! the jobs are moved from state to state by the test.

#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Read as _};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use js_proto::v1::{ExecMode, Shape};
use kbf_job_client::fake::{FakeJobServer, Machine, TOKEN};
use kbf_objstore::s3::fake::FakeS3;
use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_proto::reapi::{
    Action, BatchUpdateBlobsRequest, Command as ReapiCommand, Directory, ExecuteOperationMetadata,
    ExecuteRequest, FileNode, WaitExecutionRequest, batch_update_blobs_request,
};
use kbf_server::api_client::OperatorClient;
use kbf_testkit::dir::TestDir;
use kbf_tree_bundle::{Dir, Facts, Node, Writer, file_name};
use prost::Message as _;
use rustix::process::{Pid, Signal, kill_process};
use sha2::Digest as _;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// How long a test waits for something that should happen promptly.
const PROMPT: Duration = Duration::from_secs(10);

/// How long a test waits for a child to print its start line.
const BOUND: Duration = Duration::from_secs(60);

/// A digest-pinned image, as the executor's flags want one.
const BASE_IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// A running `kbf` child that has printed its start line; killed and reaped when dropped, so a
/// failed assertion never leaves it running.
struct Proc {
    child: Child,
    line: String,
}

impl Proc {
    /// `kbf <args>` with the test's own settings file in `dir`, once it has printed its start
    /// line.
    fn start(dir: &TestDir, args: &[String]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .arg(kbf_testkit::config::flag(dir))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf");
        // Read on a thread so that a child that never prints its start line fails the test
        // instead of hanging it.
        let stdout = child.stdout.take().expect("stdout");
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = sent.send(line);
        });
        let line = match received.recv_timeout(BOUND) {
            Ok(line) if !line.is_empty() => line,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let mut said = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut said);
                }
                panic!("kbf {args:?} printed no start line; it said:\n{said}");
            }
        };
        Self { child, line }
    }

    /// The address `key` names in the start line.
    fn address(&self, key: &str) -> SocketAddr {
        self.line
            .split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .unwrap_or_else(|| panic!("no {key} in the start line {:?}", self.line))
            .parse()
            .expect("an address")
    }

    /// Sends SIGTERM: the first of a drain, or the second that ends it.
    fn terminate(&self) {
        let pid = i32::try_from(self.child.id())
            .ok()
            .and_then(Pid::from_raw)
            .expect("a child pid");
        kill_process(pid, Signal::TERM).expect("SIGTERM sent");
    }

    /// Waits up to `within` for the child to exit; panics (and so kills it) if it has not.
    async fn exited(&mut self, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "kbf still running {within:?} after SIGTERM"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A blob of the action and its digest.
struct Blob {
    data: Vec<u8>,
    digest: kbf_proto::reapi::Digest,
}

impl Blob {
    fn of(message: &impl prost::Message) -> Self {
        Self::raw(message.encode_to_vec())
    }

    fn raw(data: Vec<u8>) -> Self {
        let hash: String = sha2::Sha256::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let digest = kbf_proto::reapi::Digest {
            hash,
            size_bytes: i64::try_from(data.len()).expect("a size"),
        };
        Self { data, digest }
    }
}

/// Runs `f` on a thread of its own and returns its result. A test database makes a runtime of
/// its own to make and to drop a database, and tokio refuses that on a thread that is running
/// tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// A database of its own on the test server with the schema the installer makes, made and
/// dropped off the runtime so that it is also dropped when a test fails.
struct Database(Option<kbf_state::postgres::TestDatabase>);

impl Database {
    fn new() -> Self {
        let database = off_runtime(|| {
            let database = kbf_state::postgres::TestDatabase::empty();
            let config = kbf_state::postgres::PostgresConfig::parse(&database.url())
                .expect("the URL parses");
            let set = kbf_pack_index::schema::embedded();
            kbf_state::postgres::schema::apply(&config, &set, kbf_pack_index::schema::latest())
                .expect("the installer's step");
            database
        });
        Self(Some(database))
    }

    /// The URL of the database, without the password that the flag refuses (the server the
    /// job runs trusts its loopback clients).
    fn url(&self) -> String {
        let url = self.0.as_ref().expect("held until dropped").url();
        let (scheme, rest) = url.split_once("://").expect("a URL");
        match rest.split_once('@') {
            Some((user, tail)) => {
                let user = user.split_once(':').map_or(user, |(name, _)| name);
                format!("{scheme}://{user}@{tail}")
            }
            None => url,
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(database) = self.0.take() {
            off_runtime(move || drop(database));
        }
    }
}

/// A frontend with an executor over a job service, in front of a storage node.
struct Rig {
    // Fields drop in order: the processes go before the directory and the stores they use.
    frontend: Proc,
    _node: Proc,
    fake: FakeJobServer,
    _packs: FakeS3,
    _database: Database,
    channel: Channel,
    api: SocketAddr,
    token: String,
    _dir: TestDir,
}

/// A Linux host of 16 cores and 64 GiB that runs containers.
fn machine() -> Machine {
    Machine {
        shape: Shape {
            cpu_millis: 16_000,
            memory_bytes: 64 << 30,
            gpus: 0,
            gpu_class: String::new(),
            disk_bytes: Some(1 << 40),
        },
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
    }
}

/// A file of mode 0600 holding `text`.
fn secret(path: &Path, text: &str) -> String {
    std::fs::write(path, format!("{text}\n")).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path.display().to_string()
}

/// Writes into `dir` the bundle of this build's release for `linux-x86_64` and the `SHA256SUMS`
/// that lists it: what `--executor-base-tree` names, and the frontend publishes at its start.
fn base_tree(dir: &Path) {
    std::fs::create_dir_all(dir).expect("a directory");
    let mut passwd = Dir::new();
    passwd.insert(
        "passwd",
        Node::file(b"root:x:0:0::/root:/bin/nologin\n", false),
    );
    let mut tools = Dir::new();
    tools.insert("kbf-task", Node::file(b"kbf-task", true));
    let mut root = Dir::new();
    root.insert("etc", Node::Dir(passwd));
    root.insert(".kbf-tools", Node::Dir(tools));
    let built = root.build().expect("a tree");
    let commit = kbf_server::BUILD_COMMIT_FULL;
    let platform = "linux-x86_64";
    let facts = Facts {
        commit: commit.to_owned(),
        platform: platform.to_owned(),
        root: built.root.clone(),
        entries: built.entries,
        depth: built.depth as u64,
        normalised: Vec::new(),
    };
    let mut writer = Writer::new(Vec::new(), &facts).expect("a bundle");
    writer
        .object(b"root:x:0:0::/root:/bin/nologin\n")
        .expect("written");
    writer.object(b"kbf-task").expect("written");
    for (_, manifest) in &built.manifests {
        writer.object(manifest).expect("written");
    }
    let bytes = writer.finish().expect("finished");
    let name = file_name(commit, platform);
    std::fs::write(dir.join(&name), &bytes).expect("a bundle");
    let sum: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(dir.join("SHA256SUMS"), format!("{sum}  {name}\n")).expect("the sums");
}

impl Rig {
    /// Starts a storage node, a job service with one host, and a frontend with the executor on
    /// it, the operator API and `more` flags.
    async fn start(name: &str, more: &[&str]) -> Self {
        let dir = TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("kbf-frontend-shutdown-{name}"),
        );
        let node = Proc::start(
            &dir,
            &[
                "storage".to_owned(),
                "--storage-listen=127.0.0.1:0".to_owned(),
                format!("--cold-store=file://{}", dir.join("cold").display()),
                "--cold-store-floor-bytes=1".to_owned(),
                format!("--meta-dir={}", dir.join("meta").display()),
            ],
        );
        base_tree(&dir.join("base-tree"));
        let database = Database::new();
        let packs = FakeS3::start().await;
        let pack_keys = dir.join("pack-keys.json");
        std::fs::write(
            &pack_keys,
            r#"{"access_key_id": "k", "secret_access_key": "s"}"#,
        )
        .expect("written");
        std::fs::set_permissions(&pack_keys, std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
        let fake = FakeJobServer::start().await.expect("a loopback port");
        fake.add_host("host-1", machine());
        let job_token = secret(&dir.join("job-token"), TOKEN);
        let token = "kbf-test-token-0123456789abcdef0123456789".to_owned();
        let api_token = secret(&dir.join("api-token"), &token);
        let task_secret = secret(&dir.join("task-secret"), &"s".repeat(40));
        let mut args = vec![
            "frontend".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
            "--api-listen=127.0.0.1:0".to_owned(),
            format!("--api-token-file={api_token}"),
            // No load balancer is in front of this frontend.
            "--drain-lb-wait=0".to_owned(),
            format!("--storage=http://{}", node.address("storage=")),
            format!("--state-store={}", database.url()),
            "--pack-index".to_owned(),
            format!("--pack-store-endpoint={}", packs.endpoint()),
            "--pack-store-bucket=kbf-fake".to_owned(),
            format!("--pack-store-credentials-file={}", pack_keys.display()),
            format!("--task-secret-file={task_secret}"),
            "--task-out-root=/mnt/ts/put".to_owned(),
            "--executor=js-agent".to_owned(),
            format!("--executor-job-url={}", fake.origin()),
            format!("--executor-job-token-file={job_token}"),
            "--executor-frontend-id=front-1".to_owned(),
            "--executor-hosts=1".to_owned(),
            "--executor-host-cpus=2".to_owned(),
            "--executor-host-memory-gib=16".to_owned(),
            "--executor-host-slots=2".to_owned(),
            format!("--executor-base-image={BASE_IMAGE}"),
            format!("--executor-kbf-image={KBF_IMAGE}"),
            format!("--executor-base-tree={}", dir.join("base-tree").display()),
            "--executor-base-tree-url=".to_owned(),
        ];
        args.extend(more.iter().map(|m| (*m).to_owned()));
        let mut frontend = Proc::start(&dir, &args);
        let reapi = frontend.address("reapi=");
        let api = frontend.address("api=");
        // The start line is printed before the listeners take calls; `/readyz` says when they do.
        let deadline = Instant::now() + BOUND;
        loop {
            let token = token.clone();
            let answer =
                tokio::task::spawn_blocking(move || try_http(api, &token, "GET", "/readyz"))
                    .await
                    .expect("the call runs");
            if answer.is_some_and(|(code, _)| code == 200) {
                break;
            }
            if Instant::now() >= deadline {
                let exit = frontend.child.try_wait();
                let mut said = String::new();
                if let Some(mut e) = frontend.child.stderr.take() {
                    let _ = frontend.child.kill();
                    let _ = e.read_to_string(&mut said);
                }
                panic!("/readyz never answered 200: {exit:?}\n{said}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect to the REAPI listener");
        Self {
            frontend,
            _node: node,
            fake,
            _packs: packs,
            _database: database,
            channel,
            api,
            token,
            _dir: dir,
        }
    }

    fn exec(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.channel.clone())
    }

    /// Uploads `blobs` to the CAS.
    async fn upload(&self, blobs: &[&Blob]) {
        let response = ContentAddressableStorageClient::new(self.channel.clone())
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: blobs
                    .iter()
                    .map(|b| batch_update_blobs_request::Request {
                        digest: Some(b.digest.clone()),
                        data: b.data.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .await
            .expect("BatchUpdateBlobs")
            .into_inner();
        for r in response.responses {
            assert_eq!(
                r.status.expect("status").code,
                0,
                "upload of {:?}",
                r.digest
            );
        }
    }

    /// An action running `argv` on one input file, every blob it names uploaded.
    async fn action(&self, argv: &str) -> Blob {
        let input = Blob::raw(format!("input of {argv}").into_bytes());
        let root = Blob::of(&Directory {
            files: vec![FileNode {
                name: "in".to_owned(),
                digest: Some(input.digest.clone()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let command = Blob::of(&ReapiCommand {
            arguments: vec![argv.to_owned()],
            ..Default::default()
        });
        let action = Blob::of(&Action {
            command_digest: Some(command.digest.clone()),
            input_root_digest: Some(root.digest.clone()),
            ..Default::default()
        });
        self.upload(&[&input, &root, &command, &action]).await;
        action
    }

    async fn execute(&self, action: &Blob) -> Result<Streaming<Operation>, tonic::Status> {
        self.exec()
            .execute(ExecuteRequest {
                instance_name: "main".to_owned(),
                action_digest: Some(action.digest.clone()),
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
    }

    /// Executes `argv` and brings its job to running on the host: the stream is open, read up
    /// to its EXECUTING update, and the operation has a running lease that never reports.
    /// Returns the stream and the operation's name.
    async fn run_on_a_host(&self, argv: &str) -> (Streaming<Operation>, String) {
        let action = self.action(argv).await;
        let mut executed = self.execute(&action).await.expect("Execute");
        let job = self.job_submitted().await;
        assert!(self.fake.place(&job, "host-1"), "placed");
        assert!(self.fake.starting(&job), "starting");
        assert!(self.fake.run(&job), "running");
        loop {
            let op = next(&mut executed).await;
            if stage(&op) == ExecStage::Executing as i32 {
                return (executed, op.name);
            }
        }
    }

    /// The id of the first job the executor submitted.
    async fn job_submitted(&self) -> String {
        let deadline = Instant::now() + PROMPT;
        loop {
            if let Some(job) = self.fake.jobs().first() {
                return job.id.clone();
            }
            assert!(Instant::now() < deadline, "no job was submitted");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A GET or POST of the operator API, off the async threads.
    async fn api(&self, method: &'static str, path: &'static str) -> (u16, String) {
        let (api, token) = (self.api, self.token.clone());
        tokio::task::spawn_blocking(move || http(api, &token, method, path))
            .await
            .expect("the call runs")
    }

    /// The answer to a GET of the operator API, waiting through a connection that is closed
    /// unanswered: a drain hands the API to its second server (`serve_api_goaway`), and a
    /// connection accepted at that moment may be closed at once.
    async fn get(&self, path: &'static str) -> (u16, String) {
        let deadline = Instant::now() + PROMPT;
        loop {
            let (api, token) = (self.api, self.token.clone());
            let answer = tokio::task::spawn_blocking(move || try_http(api, &token, "GET", path))
                .await
                .expect("the call runs");
            if let Some(answer) = answer {
                return answer;
            }
            assert!(Instant::now() < deadline, "GET {path} is not answered");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until `GET /readyz` is 503, which a stop signal or a drain makes it.
    async fn not_ready(&self) {
        let deadline = Instant::now() + PROMPT;
        while self.get("/readyz").await.0 != 503 {
            assert!(Instant::now() < deadline, "/readyz still ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until the drain reports the phase `draining`: new Execute calls are refused from
    /// then on. `/readyz` turns 503 earlier, at the start of the load balancer's wait
    /// (`lb_wait`), while new calls are still taken.
    async fn draining(&self) {
        let deadline = Instant::now() + PROMPT;
        while self.drain().await["phase"] != "draining" {
            assert!(
                Instant::now() < deadline,
                "the drain never reached `draining`"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The drain status the operator API reports.
    async fn drain(&self) -> serde_json::Value {
        let (code, body) = self.get("/v1/frontend/drain").await;
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).expect("JSON")
    }
}

/// One request to the operator API at `addr` with the operator's bearer token, and the
/// status and body of the answer; `None` when the call fails before an answer.
fn try_http(addr: SocketAddr, token: &str, method: &str, path: &str) -> Option<(u16, String)> {
    let client = OperatorClient::new(addr, Some(token.to_owned()));
    let reply = match method {
        "GET" => client.get(path),
        _ => client.post(path, ""),
    }
    .ok()?;
    Some((reply.status, reply.body))
}

/// [`try_http`], for a call that must be answered.
fn http(addr: SocketAddr, token: &str, method: &str, path: &str) -> (u16, String) {
    try_http(addr, token, method, path).expect("the operator API answers")
}

/// The stage `op` reports.
fn stage(op: &Operation) -> i32 {
    let any = op.metadata.as_ref().expect("metadata");
    ExecuteOperationMetadata::decode(any.value.as_slice())
        .expect("ExecuteOperationMetadata")
        .stage
}

/// The next update on `stream`, in time.
async fn next(stream: &mut Streaming<Operation>) -> Operation {
    tokio::time::timeout(PROMPT, stream.message())
        .await
        .expect("an update in time")
        .expect("healthy")
        .expect("an update")
}

/// Catches issue #168: a server that dies on SIGTERM (no handler), or stops serving at once, so
/// an open Execute or WaitExecution stream fails on the client with UNKNOWN (an h2 error)
/// instead of UNAVAILABLE; and one that does not exit 0 promptly once the streams are ended.
/// The deadline is 1 s here (the default is none). The operation is running as a job that
/// never ends, so the drain keeps both streams until the deadline closes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_ends_open_execute_streams_unavailable() {
    let mut rig = Rig::start("deadline", &["--shutdown-timeout-secs=1"]).await;
    let (mut executed, name) = rig.run_on_a_host("open across a SIGTERM").await;
    let mut waited = rig
        .exec()
        .wait_execution(WaitExecutionRequest { name })
        .await
        .expect("WaitExecution")
        .into_inner();
    next(&mut waited).await;

    rig.frontend.terminate();
    for (what, stream) in [("Execute", &mut executed), ("WaitExecution", &mut waited)] {
        let ended = tokio::time::timeout(PROMPT, stream.message())
            .await
            .unwrap_or_else(|_| panic!("{what}: no end within {PROMPT:?} of SIGTERM"));
        let status = ended.expect_err(what);
        assert_eq!(
            status.code(),
            Code::Unavailable,
            "{what}: {}: {}",
            status.code(),
            status.message()
        );
        assert!(
            status.message().contains("shutting down"),
            "{what}: {status:?}"
        );
    }
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches (kbf#791): a SIGTERM that ends open streams or exits while an action is in flight
/// (Buck2 never reconnects, so the action would fail). With no deadline (the default) the
/// stream of an operation with a running job stays open and the process keeps running after the
/// signal; once the frontend reports the drain begun (`/readyz` 503) and counts the stream as
/// still open, nothing has been sent on it. The defect it catches is the signal handler closing
/// the streams as it drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_keeps_a_running_stream_when_there_is_no_deadline() {
    use once::now_or_never;

    let mut rig = Rig::start("no-deadline", &[]).await;
    let (mut executed, _) = rig.run_on_a_host("kept across a SIGTERM").await;

    rig.frontend.terminate();
    rig.not_ready().await;
    let drain = rig.drain().await;
    assert_eq!(drain["draining"], true, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert_eq!(drain["queued_operations"], 0, "{drain}");
    let quiet = now_or_never(executed.message());
    assert!(quiet.is_none(), "the stream ended or spoke: {quiet:?}");
    assert!(
        rig.frontend.child.try_wait().expect("try_wait").is_none(),
        "the frontend exited"
    );

    // The client goes away and the frontend, with nothing left to wait for, exits 0.
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches: `--progress-interval-secs 0` (and `--http2-keepalive-interval-secs 0`) ignored or
/// breaking the stream: with both off the frontend still starts, takes an action and runs it as
/// a job, and when it is stopped (its deadline is 1 s) the update after the one that says the
/// action is executing is the end of the stream, UNAVAILABLE, with no update that repeats the
/// last one between them (a progress update is the last operation sent again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_turns_progress_off() {
    let mut rig = Rig::start(
        "progress-off",
        &[
            "--progress-interval-secs=0",
            "--http2-keepalive-interval-secs=0",
            "--shutdown-timeout-secs=1",
        ],
    )
    .await;
    let (mut executed, _) = rig.run_on_a_host("idle without progress").await;
    rig.frontend.terminate();
    let ended = tokio::time::timeout(PROMPT, executed.message())
        .await
        .expect("the stream ends after the signal");
    let status = ended.expect_err("the next thing on the stream is its end");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches a second SIGTERM that is swallowed (the handler of the first is all there is):
/// systemd, an operator or the rollout that waited out its drain sends another to end the
/// drain, and with no shutdown timeout (the default) nothing else would. The first SIGTERM
/// drains and the stream is kept (the status names it: one stream open, none queued: the
/// operation runs as a job); the second ends the stream UNAVAILABLE and the process exits 0.
/// The defect it catches is the escalation not being spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sigterm_ends_the_drain() {
    let mut rig = Rig::start("second-signal", &[]).await;
    let (mut executed, _) = rig.run_on_a_host("ended by a second signal").await;

    rig.frontend.terminate();
    rig.not_ready().await;
    let drain = rig.drain().await;
    assert_eq!(drain["draining"], true, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert_eq!(drain["queued_operations"], 0, "{drain}");

    rig.frontend.terminate();
    let status = tokio::time::timeout(PROMPT, executed.message())
        .await
        .expect("the stream ends after the second signal")
        .expect_err("it ends with an error");
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    assert!(status.message().contains("shutting down"), "{status:?}");
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// Catches a drain endpoint that does not drain the process: `POST /v1/frontend/drain` takes it
/// out of `/readyz`, refuses a new Execute, and keeps the open stream; the process then exits 0
/// by itself when the stream is gone, with no signal (so the run writes its coverage). The
/// defect it catches is the endpoint answering 200 and doing nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drain_endpoint_drains_the_process_and_it_exits_when_the_streams_are_done() {
    let mut rig = Rig::start("endpoint", &[]).await;
    let (executed, _) = rig.run_on_a_host("drained in the process").await;
    // The second action's inputs are uploaded before the drain: once the drain has sent the
    // connection GOAWAY, an upload on it fails too, and the test is about the Execute.
    let again = rig.action("a second action during the drain").await;

    let (code, body) = rig.api("POST", "/v1/frontend/drain").await;
    assert_eq!(code, 200, "{body}");
    rig.not_ready().await;
    rig.draining().await;
    // UNAVAILABLE from the service, or a transport error once the connection has been sent
    // GOAWAY, as the answer to the call or as the first message of its stream: either way the
    // call is not taken (the drain below still counts one open execution).
    if let Ok(mut stream) = rig.execute(&again).await {
        let first = tokio::time::timeout(PROMPT, stream.message())
            .await
            .expect("an answer in time");
        first.expect_err("a new Execute during the drain");
    }
    let drain = rig.drain().await;
    assert_eq!(drain["draining"], true, "{drain}");
    assert_eq!(drain["open_executions"], 1, "{drain}");
    assert!(
        rig.frontend.child.try_wait().expect("try_wait").is_none(),
        "the frontend exited with a stream open"
    );

    // The client goes away; the frontend has nothing left to wait for and exits 0.
    drop(executed);
    let status = rig.frontend.exited(PROMPT).await;
    assert!(status.success(), "kbf exited with {status}");
}

/// `now_or_never` without a new dependency: polls a future once.
mod once {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    pub fn now_or_never<F: Future>(future: F) -> Option<F::Output> {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(output) => Some(output),
            Poll::Pending => None,
        }
    }
}
