//! The executor over the frontend's own run preparation: the real `prepare_run` of
//! `kbf_front::prepare_run`, opened from the task flags the way a started frontend opens it,
//! and no stand-in for the key, the presigned write or the read grant. The farm is the real
//! one, the lease server is in this process, and the tier store is a memory object store the
//! test writes the task's pack to (the task itself is not run).

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_front::grants::{GrantKey, Grants};
use kbf_front::{
    Backend, Cache, CacheStorage, Described, Dispatch as _, Finished, PackSettings, Packs,
    SegmentsConfig, Stage, Submission, VerifiedBlob,
};
use kbf_lease_client::fake::{FakeLeaseServer, Pool, TOKEN};
use kbf_lease_client::{Outcome, RunId};
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use kbf_pack::{PackKeys, TaskManifest, Timing, Usage, WriterKind, write_pack};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{Action, Command, Directory, FileNode, Platform};
use kbf_sched::Request;
use kbf_server::Farm;
use kbf_server::executor::{Metrics, RunSource, Running};
use kbf_server::frontend;
use kbf_state::ManualClock;
use kbf_types::{ActionKey, Digest, LeaseKind, Qos, Resources};
use prost::Message as _;
use tonic::Code;

use crate::rig::{ClockHold, GIB, settle};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BUCKET: &str = "packs";

/// The most steps a wait takes before the test fails with what the executor did, rather than
/// waiting for ever.
const STEPS: usize = 200;

/// The log of a test, as text.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<u8>>>);

pub struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    pub fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn wire(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs()
}

fn private(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("written");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("mode");
}

/// Everything a started frontend's executor stands on, but the process: the real run
/// preparation over a CAS, the packs over a memory object store, the farm, and the executor.
struct World {
    farm: Arc<Farm>,
    fake: FakeLeaseServer,
    objects: Arc<MemoryStore>,
    metrics: Arc<Metrics>,
    running: Running,
    _hold: ClockHold,
    _dir: kbf_testkit::dir::TestDir,
}

impl World {
    /// The world, and the digests of the actions it was asked for: each is one file of the
    /// content given, held in the CAS or not.
    async fn start(files: &[(&[u8], bool)]) -> (Self, Vec<Digest>) {
        let node = Arc::new(Cache::memory());
        // The cache lingers before it writes a batch, on the clock: the blobs are stored while
        // the clock still runs by itself, before it is held.
        let mut actions = Vec::new();
        for (content, held) in files {
            actions.push(store_action(&node, content, *held).await);
        }
        let hold = ClockHold::new();
        let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-real");
        let mut pool = Pool::new("farm-linux", &["node1"], 2_000, 16 * GIB);
        pool.capabilities
            .insert("arch".to_owned(), "x86_64".to_owned());
        let fake = FakeLeaseServer::start(pool).await;
        let token = dir.path().join("lease-token");
        private(&token, TOKEN.as_bytes());
        let secret = dir.path().join("secret");
        private(&secret, &[SECRET, b"\n".as_slice()].concat());
        let presign = dir.path().join("presign.json");
        private(
            &presign,
            br#"{"access_key_id": "AKIAFAKE", "secret_access_key": "fake-secret-key"}"#,
        );

        let argv = [
            "frontend".to_owned(),
            "--storage=http://127.0.0.1:1".to_owned(),
            "--state-store=postgres:///kbf?host=/run/postgresql".to_owned(),
            "--pack-index".to_owned(),
            "--pack-store-endpoint=http://127.0.0.1:9".to_owned(),
            format!("--pack-store-bucket={BUCKET}"),
            "--pack-store-credentials-file=/nowhere".to_owned(),
            format!("--task-secret-file={}", secret.display()),
            "--task-put-endpoint=http://127.0.0.1:9300".to_owned(),
            format!("--task-put-bucket={BUCKET}"),
            format!("--task-put-credentials-file={}", presign.display()),
            "--task-read-url=https://127.0.0.1:9400".to_owned(),
            "--job-listen=127.0.0.1:9400".to_owned(),
            "--job-tls-cert=/nowhere/job.pem".to_owned(),
            "--job-tls-key=/nowhere/job.key".to_owned(),
            format!("--job-grant-secret-file={}", secret.display()),
            "--executor=js-agent".to_owned(),
            format!("--executor-lease-url={}", fake.origin()),
            format!("--executor-lease-token-file={}", token.display()),
            "--executor-frontend-id=front-1".to_owned(),
            "--executor-class=farm-linux".to_owned(),
            "--executor-hosts=1".to_owned(),
            "--executor-host-cpus=2".to_owned(),
            "--executor-host-memory-gib=16".to_owned(),
            "--executor-host-slots=2".to_owned(),
            "--executor-insecure-argv-secrets".to_owned(),
            format!("--executor-base-image={IMAGE}"),
            format!("--executor-kbf-image={KBF_IMAGE}"),
            "--executor-allow-plain-http-put".to_owned(),
        ];
        let matches = frontend::command()
            .try_get_matches_from(argv)
            .expect("flags");
        let flags = frontend::parse(&matches).expect("a frontend's flags");
        let js = flags.executor.js.expect("js-agent");

        let objects = Arc::new(MemoryStore::new(Capabilities {
            conditional_put: true,
            object_lock: false,
        }));
        let index: Arc<dyn PackIndex> =
            Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
        let packs =
            Packs::new(Arc::clone(&objects), index, keys, PackSettings::default()).expect("packs");
        let storage = Arc::new(CacheStorage::new(
            Arc::clone(&node),
            SegmentsConfig::DEFAULT,
        ));
        let backend = Backend::remote_over(storage, Some(packs.clone()));
        // The preparation a started frontend opens from its task flags, over its CAS.
        let prep = flags
            .task
            .expect("task flags")
            .open(backend.clone())
            .expect("the run preparation opens");
        let front = js.front(packs, backend, RunSource::of(&Arc::new(prep)));

        let farm = Arc::new(Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ));
        let metrics = Arc::new(Metrics::default());
        let executor = js
            .build(Arc::clone(&farm), Arc::new(front), Arc::clone(&metrics))
            .expect("an executor");
        let world = Self {
            farm,
            fake,
            objects,
            metrics,
            running: Running::spawn(executor),
            _hold: hold,
            _dir: dir,
        };
        (world, actions)
    }

    fn submit(&self, action: Digest) -> tokio::sync::watch::Receiver<Stage> {
        self.farm
            .submit(Submission {
                request: Request {
                    key: ActionKey {
                        instance: "main".to_owned(),
                        action,
                    }
                    .into(),
                    qos: Qos::Ci,
                    kind: LeaseKind::Action,
                    resources: Resources::new(1_000, GIB),
                    hermetic: true,
                    do_not_cache: false,
                    needs: kbf_caps::Request::default(),
                    whole_worker: false,
                },
                call: None,
                described: Described {
                    arguments: vec!["/bin/run".to_owned()],
                    outputs: Vec::new(),
                    label: Default::default(),
                },
                evidence: Default::default(),
                max_wall: None,
            })
            .expect("the farm takes the action")
            .stage
    }

    /// Five seconds pass on the clocks, and the farm and the executor's task run.
    async fn step(&self) {
        tokio::time::advance(Duration::from_secs(5)).await;
        self.fake.advance(Duration::from_secs(5));
        settle().await;
        self.farm.tick().await;
        settle().await;
    }

    async fn until_done(&self, stage: &tokio::sync::watch::Receiver<Stage>) -> Finished {
        for _ in 0..STEPS {
            if let Stage::Done(finished) = &*stage.borrow() {
                return finished.clone();
            }
            self.step().await;
        }
        panic!("not done after {STEPS} steps: {}", self.said());
    }

    /// What the executor counts, for a failure to say.
    fn said(&self) -> String {
        let mut counted = String::new();
        self.metrics.render(&mut counted);
        format!("{counted}\n{:?}", self.fake.requests())
    }

    /// The run the executor posts, once it has.
    async fn posted(&self) -> RunId {
        let api = self.fake.client().expect("a client");
        for _ in 0..STEPS {
            for lease in api.leases().await.expect("leases") {
                if let Some(run) = api.runs(lease.id).await.expect("runs").pop() {
                    return run.run_id;
                }
            }
            self.step().await;
        }
        panic!("no run posted after {STEPS} steps: {}", self.said());
    }
}

/// Stores `message` in `node`, and returns its digest.
async fn store(node: &Cache<kbf_front::MemoryMetaLog, MemoryStore>, message: Vec<u8>) -> Digest {
    let blob = VerifiedBlob::hashed(Bytes::from(message));
    let digest = blob.digest();
    node.store_blobs(vec![blob]).await.expect("stored");
    digest
}

/// An action whose input tree is one file, `content`, held in the CAS or (if `held` is false)
/// not.
async fn store_action(
    node: &Cache<kbf_front::MemoryMetaLog, MemoryStore>,
    content: &[u8],
    held: bool,
) -> Digest {
    let file = VerifiedBlob::hashed(Bytes::copy_from_slice(content));
    let file_digest = file.digest();
    if held {
        node.store_blobs(vec![file]).await.expect("stored");
    }
    let root = Directory {
        files: vec![FileNode {
            name: "in.txt".to_owned(),
            digest: Some(wire(&file_digest)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let root = store(node, root.encode_to_vec()).await;
    let command = store(node, Command::default().encode_to_vec()).await;
    let action = Action {
        command_digest: Some(wire(&command)),
        input_root_digest: Some(wire(&root)),
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: IMAGE.to_owned(),
            }],
        }),
        timeout: Some(prost_types::Duration {
            seconds: 90,
            nanos: 0,
        }),
        ..Action::default()
    };
    store(node, action.encode_to_vec()).await
}

/// The value after `name` in the arguments of a run.
fn arg(args: &[String], name: &str) -> String {
    let at = args.iter().position(|a| a == name).expect("the argument");
    args[at + 1].clone()
}

/// Catches the real preparation's closure size not reaching the run's disk booking: the run
/// of an action whose inputs are one 1000-byte file books the bytes of the input tree as the
/// preparation counts them (the file and the root Directory blob that names it), the default
/// output allowance and the margin. A grant that carried no size would book the allowance and
/// the margin only, which this number does not equal.
#[tokio::test(start_paused = true)]
async fn the_run_books_the_bytes_of_the_closure_the_real_preparation_found() {
    use kbf_server::executor::{DEFAULT_OUTPUT_DISK, DISK_MARGIN};

    let content = [7u8; 1000];
    let (world, actions) = World::start(&[(&content, true)]).await;
    let _stage = world.submit(actions[0]);
    let run = world.posted().await;
    let file = VerifiedBlob::hashed(Bytes::copy_from_slice(&content)).digest();
    let root = Directory {
        files: vec![FileNode {
            name: "in.txt".to_owned(),
            digest: Some(wire(&file)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let inputs = 1000 + root.encode_to_vec().len() as u64;
    assert_eq!(
        world.fake.run_disk_bytes(&run),
        Some(inputs + DEFAULT_OUTPUT_DISK + DISK_MARGIN)
    );
}

/// Catches an executor that does not run an action end to end over the real preparation, with
/// nothing standing in for the key, the presigned write or the grant: the run is posted with a
/// write of exactly the key it was issued, good for the wall and the slack, and a read grant
/// the listener's own key verifies for this lease and action; a pack under that key is ingested
/// under the same mint, and the caller gets the result. No log line, metric or settled detail
/// of the run carries the signed write or the token.
#[tokio::test(start_paused = true)]
async fn an_action_runs_and_ingests_over_the_real_run_preparation() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let _logged = tracing::dispatcher::set_default(&dispatch);

    let (world, actions) = World::start(&[(b"input", true), (b"never stored", false)]).await;
    let action = actions[0];
    let stage = world.submit(action);
    let before = unix_now();
    let run = world.posted().await;
    let after = unix_now();

    let args = world.fake.run_args(&run).expect("a run");
    let build = arg(&args, "--lease");
    let (pack_url, token) = (arg(&args, "--pack-url"), arg(&args, "--grant-token"));
    assert_eq!(arg(&args, "--grant-url"), "https://127.0.0.1:9400");
    assert_eq!(
        arg(&args, "--action"),
        format!("{}/{}", action.hash_hex(), action.size_bytes)
    );

    // The write is of exactly the key the ingest will verify, signed for the run's wall (the
    // action's timeout and the margin) and the slack.
    let (path, query) = pack_url.split_once('?').expect("a signed url");
    assert!(
        path.starts_with(&format!("http://127.0.0.1:9300/{BUCKET}/")),
        "{path}"
    );
    let key = &path[path.find("cas/b/").expect("the key")..];
    assert!(key.starts_with(&format!("cas/b/{build}/")), "{key}");
    let expires = 90 + 300 + 600;
    assert!(
        query
            .split('&')
            .any(|q| q == format!("X-Amz-Expires={expires}")),
        "{query}"
    );

    // The grant is for this lease and this action and ends with the write.
    let clock = Arc::new(move || before);
    let grants = Grants::new(GrantKey::from_secret(SECRET).expect("a key"), clock);
    let claims = grants
        .verify(&token)
        .expect("the listener's key verifies it");
    assert_eq!(claims.build, build);
    assert_eq!(claims.action, action);
    assert!(
        (before + expires..=after + expires).contains(&claims.not_after),
        "{} not in {before}+{expires}..={after}+{expires}",
        claims.not_after
    );

    // The task writes its pack under the key and exits 0.
    let manifest = TaskManifest {
        lease: build.clone(),
        action,
        exit_code: 5,
        stdout: None,
        stderr: None,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: Timing::default(),
        usage: Usage::default(),
        packs: 1,
        large: vec![],
        inputs: None,
    };
    let pack = write_pack(&manifest, &[]).expect("a pack");
    world
        .objects
        .put_new(
            &ObjectKey::new(key).expect("an object key"),
            Bytes::from(pack),
            None,
        )
        .await
        .expect("written");
    world.fake.run_started(&run);
    assert!(
        world
            .fake
            .finish_run(&run, Outcome::Succeeded, Some(0), None)
    );
    let Finished::Ran(result) = world.until_done(&stage).await else {
        panic!("the action did not run");
    };
    assert_eq!(result.exit_code, 5);

    // A second action whose input is not held is the caller's: it is refused, nothing is
    // posted for it, and the refusal says what is missing without saying anything secret.
    let refused = world.submit(actions[1]);
    let Finished::Failed(status) = world.until_done(&refused).await else {
        panic!("not refused");
    };
    assert_eq!(
        Code::from_i32(status.code),
        Code::InvalidArgument,
        "{status:?}"
    );
    assert!(status.message.contains("no key for the pack"), "{status:?}");

    let mut counted = String::new();
    world.metrics.render(&mut counted);
    for row in [
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
        "kbf_executor_ingests_total{result=\"ok\"} 1\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }
    world.running.stop().await;

    // Nothing the executor says anywhere has the signature of the write or the token.
    let signature = query
        .split('&')
        .find_map(|q| q.strip_prefix("X-Amz-Signature="))
        .expect("a signature");
    let said = [
        log.text(),
        counted,
        format!("{status:?}"),
        format!("{:?}", world.fake.requests()),
    ]
    .join("\n");
    assert!(
        said.contains("retiring the worker"),
        "the log was captured: {said}"
    );
    for secret in [signature, token.as_str(), pack_url.as_str()] {
        assert!(!said.contains(secret), "{secret} in {said}");
    }
}

/// Catches a host's failure text reaching the log: a driver's error can quote the argument
/// list, which holds the signed write and the read token, and the executor logs a run that
/// ends badly. Nothing it logs may carry either, whatever the host wrote.
#[tokio::test(start_paused = true)]
async fn a_failed_runs_detail_quoting_the_arguments_never_reaches_the_log() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish(),
    );
    let _logged = tracing::dispatcher::set_default(&dispatch);

    let (world, actions) = World::start(&[(b"input", true)]).await;
    let stage = world.submit(actions[0]);
    let run = world.posted().await;
    let args = world.fake.run_args(&run).expect("a run");
    let (pack_url, token) = (arg(&args, "--pack-url"), arg(&args, "--grant-token"));
    let signature = pack_url
        .split_once('?')
        .and_then(|(_, query)| {
            query
                .split('&')
                .find_map(|q| q.strip_prefix("X-Amz-Signature="))
        })
        .expect("a signature")
        .to_owned();

    world.fake.run_started(&run);
    let detail = format!("exec failed: --pack-url {pack_url} --grant-token {token}");
    assert!(
        world
            .fake
            .finish_run(&run, Outcome::Failed, Some(1), Some(&detail))
    );
    // The farm may run the action again after an infrastructure failure, so the stage is not
    // waited on: the executor has judged the run once it has had its turns.
    for _ in 0..10 {
        world.step().await;
    }
    drop(stage);
    world.running.stop().await;

    let said = log.text();
    assert!(
        said.contains("the task tool failed"),
        "the run was logged: {said}"
    );
    for secret in [signature.as_str(), token.as_str(), pack_url.as_str()] {
        assert!(!said.contains(secret), "{secret} in {said}");
    }
}
