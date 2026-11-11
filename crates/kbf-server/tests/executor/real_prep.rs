//! The executor over the frontend's own run preparation: the real `prepare_run` of
//! `kbf_front::prepare_run`, opened from the task flags the way a started frontend opens it,
//! and no stand-in for the key, the presigned write or the read grant. The farm is the real
//! one, the job service is in this process, and the tier store is a memory object store the
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
use kbf_job_client::fake::{FakeJobServer, TOKEN};
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

use crate::rig::{ClockHold, GIB, machine, settle};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BUCKET: &str = "packs";
/// The host the job service has, which the test places the executor's jobs on.
const HOST: &str = "host-1";

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
    fake: FakeJobServer,
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
        let fake = FakeJobServer::start().await.expect("a loopback port");
        fake.add_host(HOST, machine(64, "x86_64"));
        let token = dir.path().join("job-token");
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
            format!("--executor-job-url={}", fake.origin()),
            format!("--executor-job-token-file={}", token.display()),
            "--executor-frontend-id=front-1".to_owned(),
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
                    pin: None,
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
    /// 5 s pass, a second at a time with the executor's task let run in between (a call in
    /// flight on loopback is answered before its bound passes), and the farm ticks.
    async fn step(&self) {
        for _ in 0..5 {
            tokio::time::advance(Duration::from_secs(1)).await;
            self.fake.advance(Duration::from_secs(1));
            settle().await;
        }
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
        format!("{counted}\n{:?}", self.fake.jobs())
    }

    /// The id of the job the executor submits, once it has.
    async fn posted(&self) -> String {
        for _ in 0..STEPS {
            if let Some(job) = self.fake.jobs().first() {
                return job.id.clone();
            }
            self.step().await;
        }
        panic!("no job submitted after {STEPS} steps: {}", self.said());
    }

    /// The arguments job `id` was submitted with.
    fn args_of(&self, id: &str) -> Vec<String> {
        self.fake.submitted(id).expect("a job").args
    }

    /// The host takes the queued job `id` and runs it to its end with `exit_code`.
    fn host_finishes(&self, id: &str, exit_code: i32) {
        assert!(self.fake.place(id, HOST), "{id} is placed");
        assert!(self.fake.starting(id), "{id} is starting");
        assert!(self.fake.run(id), "{id} is running");
        assert!(self.fake.finish(id, exit_code), "{id} finishes");
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
    let job = world.posted().await;
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
        world
            .fake
            .submitted(&job)
            .and_then(|spec| spec.needs)
            .and_then(|needs| needs.disk_bytes),
        Some(inputs + DEFAULT_OUTPUT_DISK + DISK_MARGIN)
    );
}

/// Catches an executor that does not run an action end to end over the real preparation, with
/// nothing standing in for the key, the presigned write or the grant: the job is submitted with a
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
    let job = world.posted().await;
    let after = unix_now();

    let args = world.args_of(&job);
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
        objects: None,
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
    world.host_finishes(&job, 0);
    let Finished::Ran(result) = world.until_done(&stage).await else {
        panic!("the action did not run");
    };
    assert_eq!(result.exit_code, 5);

    // A second action whose input is not held is the caller's: it is refused, nothing is
    // submitted for it, and the refusal says what is missing without saying anything secret.
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
    // (The job service is not among them: the job it holds carries both, as it is sent to run.)
    let said = [log.text(), counted, format!("{status:?}")].join("\n");
    assert!(
        said.contains("retiring the worker"),
        "the log was captured: {said}"
    );
    for secret in [signature, token.as_str(), pack_url.as_str()] {
        assert!(!said.contains(secret), "{secret} in {said}");
    }
}

/// Guards the log against a failed job's text: a job's output can quote the argument list,
/// which holds the signed write and the read token, and the executor logs a job that ends
/// badly. The host writes both into the job's standard error and the job exits 1; nothing the
/// executor logs may carry either. (The job service gives a start failure no free text the
/// executor could log; the executor never reads a job's logs, so this holds, and the test keeps
/// it holding.)
#[tokio::test(start_paused = true)]
async fn a_failed_jobs_output_quoting_the_arguments_never_reaches_the_log() {
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
    let job = world.posted().await;
    let args = world.args_of(&job);
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

    assert!(world.fake.place(&job, HOST));
    assert!(world.fake.starting(&job));
    assert!(world.fake.run(&job));
    let output = format!("exec failed: --pack-url {pack_url} --grant-token {token}");
    assert!(world.fake.write_logs(&job, b"", output.as_bytes()));
    assert!(world.fake.finish(&job, 1));
    // The farm may run the action again after an infrastructure failure, so the stage is not
    // waited on: the executor has judged the job once it has had its turns.
    for _ in 0..10 {
        world.step().await;
    }
    drop(stage);
    world.running.stop().await;

    let said = log.text();
    assert!(
        said.contains("the task tool exited nonzero"),
        "the job was logged: {said}"
    );
    for secret in [signature.as_str(), token.as_str(), pack_url.as_str()] {
        assert!(!said.contains(secret), "{secret} in {said}");
    }
}
