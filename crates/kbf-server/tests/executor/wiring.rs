//! The executor as the frontend builds it: from the `--executor` flags, over the frontend's own
//! seams (the ingest of the put directories, the CAS, the directory mint), against a job
//! service in this process. An action's job is submitted with the task tool's exact command
//! line, its tree and its put directory, the pack the task committed is ingested by the
//! frontend's code, the result reaches the caller, and stopping leaves nothing to cancel.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use js_proto::v1::{NetworkLevel, mount};
use kbf_front::action_tree::{BaseFacts, BaseTree};
use kbf_front::prepare_run::{ClosureLimits, PrepareTook, collect_closure_keeping};
use kbf_front::{
    Backend, Cache, CacheStorage, Described, Dispatch as _, Finished, PackKeyMint, PackSettings,
    Packs, Recorder, SegmentsConfig, Stage, Submission, VerifiedBlob,
};
use kbf_job_client::fake::{Call, FakeJobServer, TOKEN};
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use kbf_pack::{PackKeys, TaskManifest, Timing, Usage, WriterKind, write_pack};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{Action, Command, Directory, Platform};
use kbf_server::executor::{
    Metrics, MintedDir, PrepareError, PreparedPutDir, RunPrepare, RunSource, Running, TASK_IN_TREE,
};
use kbf_server::frontend;
use kbf_state::ManualClock;
use kbf_types::{ActionKey, Digest};
use prost::Message as _;

use crate::rig::{PROMPT, machine};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The root manifest of the base tree: one symlink.
const BASE_ROOT: &[u8] =
    br#"{"v":1,"children":[{"name":"bin","kind":"symlink","target":"usr/bin"}]}"#;

/// Prepares a run as the frontend does: the input closure of the action read from the CAS, and a
/// put directory minted under the secret the ingest verifies with, good a while past the run's
/// wall.
struct Preparing {
    mint: PackKeyMint,
    backend: Backend,
    asked: Mutex<Vec<(String, Digest, Duration)>>,
}

impl Preparing {
    /// The directory of `lease`'s run of `action` and the second its keys stop being good at.
    fn directory(
        &self,
        lease: &str,
        action: &Digest,
        (now, wall): (SystemTime, Duration),
    ) -> Result<(String, u64), PrepareError> {
        self.asked
            .lock()
            .expect("asked")
            .push((lease.to_owned(), *action, wall));
        let dir = self
            .mint
            .mint_put_dir(lease, action, now)
            .map_err(|e| PrepareError::Refused(e.to_string()))?;
        let since = now.duration_since(UNIX_EPOCH).expect("after the epoch");
        Ok((dir.dir().to_owned(), since.as_secs() + wall.as_secs() + 600))
    }
}

#[tonic::async_trait]
impl RunPrepare for Preparing {
    async fn prepare_put_dir(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedPutDir, PrepareError> {
        let (dir, not_after) = self.directory(lease, action, (now, wall))?;
        let kept = collect_closure_keeping(&self.backend, action, ClosureLimits::DEFAULT).await;
        let kept = kept.map_err(|e| PrepareError::Unavailable(e.to_string()))?;
        Ok(PreparedPutDir {
            dir,
            kept,
            not_after,
            took: PrepareTook::default(),
        })
    }

    async fn mint_put_dir(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<MintedDir, PrepareError> {
        let (dir, not_after) = self.directory(lease, action, (now, wall))?;
        Ok(MintedDir { dir, not_after })
    }
}

fn wire(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// Catches an executor that does not work end to end as the frontend builds it. The flags
/// become the executor's config; the action is read from the CAS for its timeout; the job is
/// submitted with the task tool's command line, no image, the tree as its layer, the put
/// directory the preparation made mounted at `/out`, the action's wall and the frontend's label;
/// a pack the task committed in that directory is ingested by the frontend's own code into the
/// result the caller gets; and stopping the executor cancels no job, as none is left to cancel.
#[tokio::test]
async fn an_action_runs_end_to_end_through_the_flags_and_the_frontends_own_seams() {
    // The cache lingers before it writes a batch, on the clock: the blobs are stored while it
    // still runs by itself.
    let node = Arc::new(Cache::memory());
    // The action the caller sent: a command, and an action naming its image and a timeout.
    let store = |message: Vec<u8>| {
        let node = Arc::clone(&node);
        async move {
            let blob = VerifiedBlob::hashed(Bytes::from(message));
            let digest = blob.digest();
            node.store_blobs(vec![blob]).await.expect("stored");
            digest
        }
    };
    let command = store(Command::default().encode_to_vec()).await;
    let root = store(Directory::default().encode_to_vec()).await;
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
    let action = store(action.encode_to_vec()).await;

    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-wiring");
    let fake = FakeJobServer::start().await.expect("a loopback port");
    fake.add_host("host-1", machine(64, "x86_64"));
    let token = dir.path().join("job-token");
    std::fs::write(&token, TOKEN).expect("written");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("mode");

    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        "--state-store=postgres:///kbf?host=/run/postgresql".to_owned(),
        "--pack-index".to_owned(),
        "--pack-store-endpoint=http://127.0.0.1:9".to_owned(),
        "--pack-store-bucket=packs".to_owned(),
        "--pack-store-credentials-file=/nowhere".to_owned(),
        // What the flags need beside the executor: only parsed here, the test prepares its own
        // runs.
        "--task-secret-file=/nowhere/secret".to_owned(),
        "--task-out-root=/mnt/ts/put".to_owned(),
        "--executor=js-agent".to_owned(),
        format!("--executor-job-url={}", fake.origin()),
        format!("--executor-job-token-file={}", token.display()),
        "--executor-frontend-id=front-1".to_owned(),
        format!("--executor-base-image={IMAGE}"),
        format!("--executor-kbf-image={KBF_IMAGE}"),
        "--executor-base-tree=/nowhere/base-tree".to_owned(),
        "--executor-base-tree-url=".to_owned(),
    ];
    let matches = frontend::command()
        .try_get_matches_from(argv)
        .expect("flags");
    let flags = frontend::parse(&matches).expect("a frontend's flags");
    let mut js = flags.executor.js.expect("js-agent");
    js.config.poll = Duration::from_millis(5);
    // The base tree the frontend publishes before it starts the executor.
    let base = BaseTree::new(
        BASE_ROOT,
        BaseFacts {
            entries: 1,
            depth: 1,
        },
    )
    .expect("a base");
    js.published.set(base).expect("set once");

    // The frontend's seams: packs over an object store, the CAS of a node, the directory mint.
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
    let preparing = Arc::new(Preparing {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        backend: backend.clone(),
        asked: Mutex::new(Vec::new()),
    });
    let source = RunSource {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        prepare: preparing.clone(),
    };
    let front = js.front(packs, backend, source);

    let metrics = Arc::new(Metrics::default());
    let executor = js
        .build(
            Arc::new(front),
            Arc::clone(&metrics),
            None,
            Recorder::off(),
            Duration::from_secs(600),
        )
        .expect("an executor");
    let running = Running::spawn(&executor);

    let ticket = executor
        .submit(Submission {
            key: ActionKey {
                instance: "main".to_owned(),
                action,
            },
            whole_machine: false,
            call: None,
            described: Described {
                arguments: vec!["/bin/run".to_owned()],
                outputs: Vec::new(),
                label: Default::default(),
            },
            evidence: Default::default(),
        })
        .expect("the executor takes the action");
    let mut stage = ticket.stage;

    // The job is submitted.
    let submitted = async {
        loop {
            if let Some(job) = fake.jobs().first() {
                return (job.id.clone(), fake.submitted(&job.id).expect("a job"));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let (job, spec) = tokio::time::timeout(PROMPT, submitted)
        .await
        .expect("the job is submitted");
    let (build, asked_action, wall) = preparing.asked.lock().expect("asked")[0].clone();
    assert_eq!(asked_action, action);
    assert_eq!(
        wall,
        Duration::from_secs(90 + 300),
        "the action's timeout and the margin"
    );
    let source = spec.mounts.first().and_then(|m| m.source.clone());
    let Some(mount::Source::HostPath(out)) = source else {
        panic!("no host directory mounted: {:?}", spec.mounts);
    };
    let pack_dir = out
        .strip_prefix("/mnt/ts/put/")
        .expect("under the put root")
        .to_owned();
    assert!(
        pack_dir.starts_with(&format!("cas/b/{build}/")),
        "{pack_dir}"
    );
    let expected: Vec<String> = [
        "--tree",
        "--stage-and-commit",
        "--out-dir",
        "/out",
        "--action",
        &format!("{}/{}", action.hash_hex(), action.size_bytes),
        "--lease",
        &build,
        "--deadline-ms",
        "90000",
        "--object-format",
        "one-pass",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(spec.args, expected);
    // The job has no image: the tree is its root, and the task tool in it is its command. It
    // has no file and no network.
    assert_eq!(spec.image, "");
    assert_eq!(spec.command, TASK_IN_TREE);
    assert!(spec.files.is_empty(), "{:?}", spec.files);
    assert_eq!(spec.network(), NetworkLevel::None);
    assert_eq!(spec.wall_limit_seconds, Some(390));
    assert_eq!(
        spec.labels.get("kbf-frontend").map(String::as_str),
        Some("front-1")
    );

    // The task commits its pack in the directory it was issued, and exits 0: the action ran,
    // and exited 5.
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
    let object = ObjectKey::new(format!("{pack_dir}/pack")).expect("an object key");
    objects
        .put_new(&object, Bytes::from(pack), None)
        .await
        .expect("written");
    assert!(fake.place(&job, "host-1"));
    assert!(fake.starting(&job));
    assert!(fake.run(&job));
    assert!(fake.finish(&job, 0));

    let done = tokio::time::timeout(
        PROMPT,
        stage.wait_for(|stage| matches!(stage, Stage::Done(_))),
    )
    .await;
    let Stage::Done(finished) = done.expect("answered").expect("open").clone() else {
        unreachable!("waited for done")
    };
    let Finished::Ran(result) = finished else {
        panic!("not ran: {finished:?}");
    };
    assert_eq!(result.exit_code, 5);

    let mut counted = String::new();
    metrics.render(&mut counted);
    for row in [
        "kbf_executor_jobs_submitted_total 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
        "kbf_executor_ingests_total{result=\"ok\"} 1\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }

    running.stop().await;
    let cancelled = fake
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::CancelJob(_)))
        .count();
    assert_eq!(
        cancelled, 0,
        "the job had ended: nothing was left to cancel"
    );
}
