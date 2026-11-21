//! A farm that one frontend is updated in: two frontends started in this process as a start is
//! (`start_frontend`, so each runs the real drain sequence and the real executor), over one
//! Postgres database, one storage node, one job service and one pack store. The job service
//! ([`FakeJobServer`], one Linux host) and the pack store ([`FakeS3`]) run in this process; the
//! test moves each job from state to state and, for a job that ends, commits the task's pack
//! where the executor reads it, as the task tool does on a host.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use js_proto::v1::{ExecMode, Shape, mount};
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_job_client::fake::{FakeJobServer, Machine, TOKEN};
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore as _};
use kbf_pack::{TaskManifest, Timing, Usage, write_pack};
use kbf_server::drain::Phase;
use kbf_server::frontend::{self, start_frontend};
use kbf_server::{BoundStorage, Drain, ServeError, StorageListener, bind_storage};
use kbf_state::postgres::TestDatabase;
use kbf_storage::Storage;
use kbf_testkit::dir::TestDir;
use kbf_tree_bundle::{Dir, Facts, Node, Writer, file_name};
use sha2::Digest as _;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::support::{Blob, Client, PROMPT};

/// The base environment the executor serves, pinned by digest as its flag wants.
const BASE_IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The kbf image the jobs launch from.
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Where a job's put directory is mounted from on a host (`--task-out-root`).
const PUT_ROOT: &str = "/mnt/ts/put/";

/// The bucket the fake pack store's own client writes to, which the frontends are given.
const BUCKET: &str = "tierstore-fake";

/// What every action of these tests prints: the one blob its task's pack holds.
const STDOUT: &[u8] = b"done\n";

/// The operator API's bearer token.
const API_TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// Runs `f` on a thread of its own and returns its result. A test database makes a runtime of
/// its own to make and to drop a database, and tokio refuses that on a thread that is running
/// tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// A database of its own on the test server with the schema a release embeds (the pack index's
/// tables among them), made and dropped off the runtime, so it is also dropped when a test fails.
struct Database(Option<TestDatabase>);

impl Database {
    fn new() -> Self {
        Self(Some(off_runtime(kbf_pack_index::schema::test_database)))
    }

    fn url(&self) -> String {
        self.0.as_ref().expect("held until dropped").url()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(database) = self.0.take() {
            off_runtime(move || drop(database));
        }
    }
}

/// A file of mode 0600 holding `text`.
fn secret(path: &Path, text: &str) -> String {
    std::fs::write(path, format!("{text}\n")).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path.display().to_string()
}

/// Writes into `dir` the bundle of this build's release for `linux-x86_64` and the `SHA256SUMS`
/// that lists it: what `--executor-base-tree` names, and a frontend publishes at its start.
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
    let (commit, platform) = (kbf_server::BUILD_COMMIT_FULL, "linux-x86_64");
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
        xcodes: Vec::new(),
    }
}

/// A plain-text storage node over an in-memory cache, serving until the farm is dropped.
async fn storage_node() -> (SocketAddr, oneshot::Sender<()>) {
    let cache = Arc::new(Cache::memory());
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(cache, SegmentsConfig::DEFAULT));
    let (stop, stopped) = oneshot::channel::<()>();
    let BoundStorage { addr, serving } = bind_storage(
        storage,
        StorageListener {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            tls: None,
            service: kbf_storage::service::Config {
                write_bytes_in_flight: 64 << 20,
            },
        },
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bind the node");
    tokio::spawn(serving);
    (addr, stop)
}

/// The services every frontend of the farm shares.
pub struct Farm {
    /// The job service.
    pub jobs: FakeJobServer,
    packs: FakeS3,
    node: SocketAddr,
    _node_stop: oneshot::Sender<()>,
    dir: TestDir,
    /// Declared last, so the database is dropped after everything that holds it.
    database: Database,
}

impl Farm {
    /// Starts the job service (one host), the pack store, the storage node and a database.
    pub async fn start(name: &str) -> Self {
        let jobs = FakeJobServer::start().await.expect("a loopback port");
        jobs.add_host("host-1", machine());
        let packs = FakeS3::start().await;
        let (node, node_stop) = storage_node().await;
        let dir = TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            &format!("kbf-frontend-update-{name}"),
        );
        base_tree(&dir.path().join("base-tree"));
        Self {
            jobs,
            packs,
            node,
            _node_stop: node_stop,
            dir,
            database: Database::new(),
        }
    }

    /// Starts a frontend over the farm whose executor is named `id` (each its own, as each
    /// frontend of a cell is). Its load balancer wait lasts a day, so a test ends it by hand.
    pub fn front(&self, id: &str) -> Front {
        let dir = self.dir.path();
        let keys = dir.join("pack-keys.json");
        std::fs::write(&keys, r#"{"access_key_id": "k", "secret_access_key": "s"}"#)
            .expect("written");
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let argv = [
            "frontend".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
            "--api-listen=127.0.0.1:0".to_owned(),
            format!(
                "--api-token-file={}",
                secret(&dir.join("api-token"), API_TOKEN)
            ),
            format!("--storage=http://{}", self.node),
            // Parsed only: the URL of the test database holds a password, which the flag
            // refuses; the URL is set below.
            "--state-store=postgres:///kbf?host=/run/postgresql".to_owned(),
            "--drain-lb-wait=86400".to_owned(),
            "--readyz-probe-interval-ms=50".to_owned(),
            "--pack-index".to_owned(),
            format!("--pack-store-endpoint={}", self.packs.endpoint()),
            format!("--pack-store-bucket={BUCKET}"),
            format!("--pack-store-credentials-file={}", keys.display()),
            format!(
                "--task-secret-file={}",
                secret(&dir.join("task-secret"), &"s".repeat(40))
            ),
            format!("--task-out-root={}", PUT_ROOT.trim_end_matches('/')),
            "--executor=js-agent".to_owned(),
            format!("--executor-job-url={}", self.jobs.origin()),
            format!(
                "--executor-job-token-file={}",
                secret(&dir.join("job-token"), TOKEN)
            ),
            format!("--executor-frontend-id={id}"),
            format!("--executor-base-image={BASE_IMAGE}"),
            format!("--executor-kbf-image={KBF_IMAGE}"),
            format!("--executor-base-tree={}", dir.join("base-tree").display()),
            "--executor-base-tree-url=".to_owned(),
            "--executor-poll-ms=100".to_owned(),
        ];
        let matches = frontend::command().try_get_matches_from(argv);
        let mut flags = frontend::parse(&matches.expect("flags")).expect("a frontend's flags");
        flags.server.state_store = Some(self.database.url().parse().expect("a postgres URL"));
        Front::start(&flags)
    }

    /// The id of the job submitted for `action`, once one is.
    pub async fn job_of(&self, action: &Blob) -> String {
        let named = format!("{}/{}", action.digest.hash_hex(), action.digest.size_bytes);
        let submitted = async {
            loop {
                let of = self.jobs.jobs().into_iter().find(|job| {
                    let spec = self.jobs.submitted(&job.id);
                    spec.is_some_and(|spec| spec.args.contains(&named))
                });
                if let Some(job) = of {
                    return job.id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(PROMPT, submitted)
            .await
            .expect("the job of the action is submitted")
    }

    /// Places `job` on the host and runs it.
    pub fn runs(&self, job: &str) {
        assert!(self.jobs.place(job, "host-1"), "placed");
        assert!(self.jobs.starting(job), "starting");
        assert!(self.jobs.run(job), "running");
    }

    /// Ends `job`, a run of `action`, as a run that exited 0: its task commits its pack in the
    /// put directory the job was given, and the job ends 0.
    pub async fn finishes(&self, job: &str, action: &Blob) {
        let spec = self.jobs.submitted(job).expect("a submitted job");
        let source = spec.mounts.first().and_then(|m| m.source.clone());
        let Some(mount::Source::HostPath(out)) = source else {
            panic!("no host directory mounted: {:?}", spec.mounts);
        };
        let put_dir = out.strip_prefix(PUT_ROOT).expect("under the put root");
        let lease = spec
            .args
            .iter()
            .skip_while(|arg| *arg != "--lease")
            .nth(1)
            .expect("the job's lease")
            .clone();
        let manifest = TaskManifest {
            lease,
            action: action.digest,
            exit_code: 0,
            stdout: Some(kbf_segments::sha256(STDOUT)),
            stderr: None,
            output_files: Vec::new(),
            output_directories: Vec::new(),
            output_symlinks: Vec::new(),
            timing: Timing::default(),
            usage: Usage::default(),
            packs: 1,
            large: vec![],
            inputs: None,
            objects: None,
        };
        let pack = write_pack(&manifest, &[STDOUT]).expect("a pack");
        let store = self.packs.store(Capabilities {
            conditional_put: true,
            object_lock: false,
        });
        let key = ObjectKey::new(format!("{put_dir}/pack")).expect("an object key");
        store
            .put_new(&key, Bytes::from(pack), None)
            .await
            .expect("the pack is committed");
        assert!(self.jobs.finish(job, 0), "finished");
    }
}

/// A frontend serving in the test, until its stop signal is given.
pub struct Front {
    pub reapi: SocketAddr,
    worker: SocketAddr,
    /// The frontend's drain: the events of its phases, and the end of its load balancer wait.
    pub drain: Arc<Drain>,
    stop: Option<oneshot::Sender<()>>,
    served: JoinHandle<Result<(), ServeError>>,
    /// Becomes true when the serving future has returned: the process would exit.
    ended: watch::Receiver<bool>,
}

impl Front {
    fn start(flags: &frontend::FrontendFlags) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let handle = start_frontend(flags, || {
            Ok(async move {
                let _ = stopped.await;
            })
        })
        .expect("starts");
        let (reapi, worker) = (handle.reapi, handle.worker.expect("a worker listener"));
        let drain = handle.drain();
        let (finish, ended) = watch::channel(false);
        let serving = async move {
            let served = handle.serve().await;
            finish.send_replace(true);
            served
        };
        Self {
            reapi,
            worker,
            drain,
            stop: Some(stop),
            served: tokio::spawn(serving),
            ended,
        }
    }

    /// A client of the frontend on a connection of its own, as a load balancer gives a client
    /// that connects anew. It uploads the empty blob, which the result of every action of these
    /// tests names (its empty output streams) and an ingest wants held.
    pub async fn client(&self) -> Client {
        let client = Client::connect(self.reapi, self.worker).await;
        client.upload(&[&Blob::new(Vec::<u8>::new())]).await;
        client
    }

    /// Gives the frontend its stop signal, as SIGTERM does.
    pub fn signal(&mut self) {
        drop(self.stop.take());
    }

    /// Waits until the drain is in `phase`, for at most [`PROMPT`] (a hang guard only).
    pub async fn reaches(&self, phase: Phase) {
        let mut phases = self.drain.phases();
        let reached = tokio::time::timeout(PROMPT, phases.wait_for(|now| *now == phase));
        let seen = reached.await.expect("the phase is reached in time");
        seen.expect("the drain is alive");
    }

    /// Whether the serving future has returned, as the process exits when it does.
    pub fn has_ended(&self) -> bool {
        *self.ended.borrow()
    }

    /// Waits until the frontend has stopped serving, for at most [`PROMPT`] (a hang guard), and
    /// returns how it ended.
    pub async fn exits(self) -> Result<(), ServeError> {
        let ended = tokio::time::timeout(PROMPT, self.served).await;
        ended
            .expect("the frontend exits in time")
            .expect("its task")
    }
}
