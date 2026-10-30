//! The executor as the frontend builds it: from the `--executor` flags, over the frontend's own
//! seams (the ingest of the packs, the CAS, the key mint) and the real farm, against a lease
//! server in this process. An action is placed, its run is posted with the task tool's exact
//! command line and mounts, the pack the task wrote is ingested by the frontend's code, the
//! result reaches the caller, and stopping releases the lease.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use kbf_front::{
    Backend, Cache, CacheStorage, Described, Dispatch as _, Finished, PackKeyMint, PackSettings,
    Packs, SegmentsConfig, Stage, Submission, VerifiedBlob,
};
use kbf_lease_client::fake::{FakeLeaseServer, Pool, TOKEN};
use kbf_lease_client::{LeaseState, Outcome};
use kbf_objstore::{Capabilities, MemoryStore, ObjectKey, ObjectStore as _};
use kbf_pack::{PackKeys, Slot, TaskManifest, Timing, Usage, WriterKind, write_pack};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{Action, Command, Platform};
use kbf_sched::Request;
use kbf_server::Farm;
use kbf_server::executor::{
    Metrics, PrepareError, PreparedRun, RunPrepare, RunSource, Running, Secret,
};
use kbf_server::frontend;
use kbf_state::ManualClock;
use kbf_types::{ActionKey, Digest, LeaseKind, Qos, Resources};
use prost::Message as _;

use crate::rig::{ClockHold, GIB, settle};

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const KBF_IMAGE: &str =
    "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Prepares a run as the frontend does: a key minted under the secret the ingest verifies with,
/// one write url for exactly that key, and a grant, good a while past the run's wall.
struct Preparing {
    mint: PackKeyMint,
    asked: Mutex<Vec<(String, Digest, Duration)>>,
}

#[tonic::async_trait]
impl RunPrepare for Preparing {
    async fn prepare_run(
        &self,
        lease: &str,
        action: &Digest,
        now: SystemTime,
        wall: Duration,
    ) -> Result<PreparedRun, PrepareError> {
        self.asked
            .lock()
            .expect("asked")
            .push((lease.to_owned(), *action, wall));
        let key = self
            .mint
            .mint(lease, action, now, Slot::Pack(0))
            .map_err(|e| PrepareError::Refused(e.to_string()))?;
        let since = now.duration_since(UNIX_EPOCH).expect("after the epoch");
        Ok(PreparedRun {
            put_url: Secret::new(format!("http://tier.test/{}?sig=put", key.key())),
            key: key.key().to_owned(),
            grant_url: "https://grant.test:9400".to_owned(),
            grant_token: Secret::new("read-token"),
            not_after: since.as_secs() + wall.as_secs() + 600,
            input_bytes: None,
        })
    }
}

fn wire(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// Catches an executor that does not work end to end as the frontend builds it. The flags
/// become the executor's config; the action is read from the CAS for its image and timeout; the
/// run is posted with the task tool's command line, the secrets the preparation made, from
/// the kbf image and with nothing mounted; a pack the task wrote under the issued key is ingested
/// by the frontend's own code into the result the caller gets; and stopping releases the lease.
#[tokio::test(start_paused = true)]
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
    let action = Action {
        command_digest: Some(wire(&command)),
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

    let _hold = ClockHold::new();
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-wiring");
    let mut pool = Pool::new("farm-linux", &["node1"], 2_000, 16 * GIB);
    pool.capabilities
        .insert("arch".to_owned(), "x86_64".to_owned());
    let fake = FakeLeaseServer::start(pool).await;
    let api = fake.client().expect("a client");
    let token = dir.path().join("lease-token");
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
        // runs (`real_prep.rs` opens the real ones).
        "--task-secret-file=/nowhere/secret".to_owned(),
        "--task-put-endpoint=http://127.0.0.1:9300".to_owned(),
        "--task-put-bucket=packs".to_owned(),
        "--task-put-credentials-file=/nowhere/presign.json".to_owned(),
        "--task-read-url=https://127.0.0.1:9400".to_owned(),
        "--job-listen=127.0.0.1:9400".to_owned(),
        "--job-tls-cert=/nowhere/job.pem".to_owned(),
        "--job-tls-key=/nowhere/job.key".to_owned(),
        "--job-grant-secret-file=/nowhere/secret".to_owned(),
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
        "--executor-work-dir=/scratch".to_owned(),
    ];
    let matches = frontend::command()
        .try_get_matches_from(argv)
        .expect("flags");
    let flags = frontend::parse(&matches).expect("a frontend's flags");
    let js = flags.executor.js.expect("js-agent");

    // The frontend's seams: packs over an object store, the CAS of a node, the key mint.
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
    let preparing = Arc::new(Preparing {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        asked: Mutex::new(Vec::new()),
    });
    let source = RunSource {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        prepare: preparing.clone(),
    };
    let backend = Backend::remote_over(storage, Some(packs.clone()));
    let front = js.front(packs, backend, source);

    let farm = Arc::new(Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let metrics = Arc::new(Metrics::default());
    let executor = js
        .build(Arc::clone(&farm), Arc::new(front), Arc::clone(&metrics))
        .expect("an executor");
    let running = Running::spawn(executor);

    let ticket = farm
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
        .expect("the farm takes the action");
    let stage = ticket.stage;

    // The run is posted.
    let posted = loop {
        let leases = api.leases().await.expect("leases");
        let mut runs = Vec::new();
        for lease in leases {
            runs.extend(api.runs(lease.id).await.expect("runs"));
        }
        if let Some(run) = runs.pop() {
            break run;
        }
        step(&farm, &fake).await;
    };
    let (build, asked_action, wall) = preparing.asked.lock().expect("asked")[0].clone();
    assert_eq!(asked_action, action);
    assert_eq!(
        wall,
        Duration::from_secs(90 + 300),
        "the action's timeout and the margin"
    );
    let key = format!("cas/b/{build}/");
    let args = fake.run_args(&posted.run_id).expect("a run");
    let url = args[args.iter().position(|a| a == "--pack-url").expect("a url") + 1].clone();
    let pack_key = url
        .strip_prefix("http://tier.test/")
        .and_then(|rest| rest.split('?').next())
        .expect("the preparation's url")
        .to_owned();
    assert!(pack_key.starts_with(&key), "{pack_key}");
    let expected: Vec<String> = [
        "--action",
        &format!("{}/{}", action.hash_hex(), action.size_bytes),
        "--lease",
        &build,
        "--grant-url",
        "https://grant.test:9400",
        "--deadline-ms",
        "90000",
        "--insecure-argv-secrets",
        "--pack-url",
        &format!("http://tier.test/{pack_key}?sig=put"),
        "--grant-token",
        "read-token",
        "--work-dir",
        "/scratch",
        "--allow-plain-http-put",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(args, expected);
    // The run is launched from the kbf image, not the base the action names, with nothing
    // mounted: the task tool is that image's entrypoint.
    assert_eq!(fake.run_image(&posted.run_id).as_deref(), Some(KBF_IMAGE));
    assert_eq!(fake.run_mounts(&posted.run_id), Some(Vec::new()));
    assert_eq!(fake.run_wall_ms(&posted.run_id), Some(390_000));

    // The task writes its pack under the key it was issued, and exits 0: the action ran, and
    // exited 5.
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
    let object = ObjectKey::new(pack_key).expect("an object key");
    objects
        .put_new(&object, Bytes::from(pack), None)
        .await
        .expect("written");
    fake.run_started(&posted.run_id);
    assert!(fake.finish_run(&posted.run_id, Outcome::Succeeded, Some(0), None));

    let finished = loop {
        if let Stage::Done(finished) = &*stage.borrow() {
            break finished.clone();
        }
        step(&farm, &fake).await;
    };
    let Finished::Ran(result) = finished else {
        panic!("not ran: {finished:?}");
    };
    assert_eq!(result.exit_code, 5);

    let mut counted = String::new();
    metrics.render(&mut counted);
    for row in [
        "kbf_executor_leases_held 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
        "kbf_executor_ingests_total{result=\"ok\"} 1\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }

    running.stop().await;
    let leases = api.leases().await.expect("leases");
    assert!(
        leases.iter().all(|l| l.state == LeaseState::Releasing),
        "released: {leases:?}"
    );
    let release = format!("DELETE /v1/leases/{}", leases[0].id);
    assert!(fake.requests().contains(&release), "{:?}", fake.requests());
}

/// Five seconds pass on the clocks, and the farm and the executor's task run.
async fn step(farm: &Farm, fake: &FakeLeaseServer) {
    tokio::time::advance(Duration::from_secs(5)).await;
    fake.advance(Duration::from_secs(5));
    settle().await;
    farm.tick().await;
    settle().await;
}
