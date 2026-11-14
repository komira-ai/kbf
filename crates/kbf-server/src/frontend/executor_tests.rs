//! A frontend started with the executor flags, over a job service in this process: with no
//! executor it never contacts the job service; with `js-agent` it registers its workers while
//! it serves, takes up the jobs an earlier process left and cancels its own when it stops, and
//! stops by itself when another frontend has jobs.

use std::error::Error;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use js_proto::v1::{ExecMode, JobSpec, JobState, ListJobsRequest, Shape};
use kbf_front::PackKeyMint;
use kbf_job_client::fake::{Call, FakeJobServer, Machine, TOKEN};
use kbf_objstore::s3::fake::{Failure, FakeS3};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use super::base_tree::tests::bundle_dir;
use super::pack_tests::{flags_over, index, pack_flags, scrape};
use super::{FrontendFlags, FrontendHandle, Prepared, boxed};
use crate::executor::{PrepareError, PreparedRun, RunPrepare, RunSource};
use crate::serve::ServeError;
use crate::test_dir::scratch_dir;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

/// A preparation that is never asked: no action is placed in these tests.
pub(super) struct Idle;

#[tonic::async_trait]
impl RunPrepare for Idle {
    async fn prepare_run(
        &self,
        _: &str,
        _: &kbf_types::Digest,
        _: std::time::SystemTime,
        _: std::time::Duration,
    ) -> Result<PreparedRun, PrepareError> {
        Err(PrepareError::Unavailable("not prepared here".to_owned()))
    }
}

pub(super) fn source() -> RunSource {
    RunSource {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        prepare: Arc::new(Idle),
    }
}

/// A job service with the hosts `hosts`, each a machine that fits every job the executor
/// submits.
pub(super) async fn job_server(hosts: &[&str]) -> FakeJobServer {
    let fake = FakeJobServer::start().await.expect("a job service");
    for host in hosts {
        fake.add_host(host, machine());
    }
    fake
}

/// A Linux host of 16 cores, 64 GiB and 1 TiB of disk, running containers.
pub(super) fn machine() -> Machine {
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

/// The flags the host executor prepares its runs with, over files in `dir`: the task flags and
/// the job-facing read listener (on loopback, with a certificate that names it), which share
/// one secret, as they do in a deployment.
fn beside_flags(dir: &Path) -> Vec<String> {
    let private = |name: &str, bytes: &[u8]| {
        let file = dir.join(name);
        std::fs::write(&file, bytes).expect("written");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
        file.display().to_string()
    };
    let secret = private(
        "task.secret",
        b"0123456789abcdef0123456789abcdef-the-farm\n",
    );
    let presign = private(
        "presign.json",
        br#"{"access_key_id": "k", "secret_access_key": "s"}"#,
    );
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(
        std::net::Ipv4Addr::LOCALHOST.into(),
    )];
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = params.self_signed(&key).expect("certificate");
    let cert = private("job.pem", cert.pem().as_bytes());
    let key = private("job.key", key.serialize_pem().as_bytes());
    vec![
        format!("--task-secret-file={secret}"),
        "--task-put-endpoint=http://127.0.0.1:9".to_owned(),
        "--task-put-bucket=kbf-fake".to_owned(),
        format!("--task-put-credentials-file={presign}"),
        "--task-read-url=https://127.0.0.1:9400".to_owned(),
        "--job-listen=127.0.0.1:0".to_owned(),
        format!("--job-tls-cert={cert}"),
        format!("--job-tls-key={key}"),
        format!("--job-grant-secret-file={secret}"),
        "--executor-allow-plain-http-put".to_owned(),
    ]
}

/// The flags of a frontend with packs on the S3 service at `endpoint`, and the executor flags
/// `executor`, which name the job service `fake`.
fn flags(dir: &Path, s3: &FakeS3, fake: &FakeJobServer, executor: &[&str]) -> FrontendFlags {
    flags_with(
        Some("--storage=http://127.0.0.1:1"),
        dir,
        s3,
        fake,
        executor,
    )
}

/// [`flags`] with the `--storage` flag `storage` (none: a frontend with no storage node).
pub(super) fn flags_with(
    storage: Option<&str>,
    dir: &Path,
    s3: &FakeS3,
    fake: &FakeJobServer,
    executor: &[&str],
) -> FrontendFlags {
    flags_over(storage, &flag_list(dir, s3, fake, executor))
}

/// The flags after the listeners and the state store of a frontend with packs on `s3` and the
/// executor flags `executor`, which name the job service `fake`.
pub(super) fn flag_list(
    dir: &Path,
    s3: &FakeS3,
    fake: &FakeJobServer,
    executor: &[&str],
) -> Vec<String> {
    let token = dir.join("job-token");
    std::fs::write(&token, TOKEN).expect("written");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let base_tree = dir.join("base-tree");
    std::fs::create_dir_all(&base_tree).expect("a directory");
    bundle_dir(&base_tree, crate::BUILD_COMMIT_FULL, &["linux-x86_64"]);
    let mut more = pack_flags(s3.endpoint(), dir);
    more.extend(beside_flags(dir));
    more.extend([
        format!("--executor-base-tree={}", base_tree.display()),
        format!("--executor-job-url={}", fake.origin()),
        format!("--executor-job-token-file={}", token.display()),
        "--executor-frontend-id=front-1".to_owned(),
        "--executor-hosts=1".to_owned(),
        "--executor-host-cpus=2".to_owned(),
        "--executor-host-memory-gib=16".to_owned(),
        "--executor-base-image=registry.test/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        "--executor-kbf-image=registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_owned(),
    ]);
    more.extend(executor.iter().map(ToString::to_string));
    more
}

/// Starts the frontend `flags` describe, which stops when `stop` is sent to.
fn started(
    flags: &FrontendFlags,
    source: Option<RunSource>,
) -> Result<(FrontendHandle, oneshot::Sender<()>), Box<dyn Error>> {
    started_over(flags, source, None)
}

/// [`started`] over the state store `state`, if any.
fn started_over(
    flags: &FrontendFlags,
    source: Option<RunSource>,
    state: Option<crate::StateStore>,
) -> Result<(FrontendHandle, oneshot::Sender<()>), Box<dyn Error>> {
    let (stop, stopped) = oneshot::channel::<()>();
    let shutdown = boxed(move || {
        Ok(async move {
            let _ = stopped.await;
        })
    });
    let handle = Prepared::new(flags)?.start(state, Some(index()), shutdown, source)?;
    Ok((handle, stop))
}

/// A state store in this process, and the handle the frontend is started over.
pub(super) fn state_store() -> (Arc<Fake>, crate::StateStore) {
    let fake = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = crate::StateStore {
        store: fake.clone(),
        epoch: crate::farm::EpochPolicy::Keep,
        members: crate::members::MembershipConfig::default(),
        history: None,
    };
    (fake, state)
}

/// The keys of the pack store under `cas/l/`.
fn large_keys(s3: &FakeS3) -> Vec<String> {
    let all = s3.keys();
    all.into_iter()
        .filter(|k| k.starts_with("cas/l/"))
        .collect()
}

/// The whole answer of `GET path` on the operator listener at `api`, or `None` when the
/// listener refused, reset or closed the connection before it answered.
pub(super) async fn try_get(api: std::net::SocketAddr, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(api).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).await.ok()?;
    Some(text)
}

/// Waits until the farm lists the worker `worker`, as a registered node. A connection the
/// listener drops (a frontend that is stopping closes it) is not an answer, and is asked
/// again: whether the frontend stops is for its `serve` to say, not for this poll.
pub(super) async fn registered(api: std::net::SocketAddr, worker: &str) -> String {
    loop {
        if let Some(nodes) = try_get(api, "/v1/nodes").await
            && nodes.contains(worker)
        {
            return nodes;
        }
        tokio::task::yield_now().await;
    }
}

/// The calls the job service has handled, by method.
pub(super) fn methods(fake: &FakeJobServer) -> Vec<&'static str> {
    fake.calls()
        .iter()
        .map(|call| match call {
            Call::SubmitJob(_) => "SubmitJob",
            Call::GetJob(_) => "GetJob",
            Call::ListJobs(_) => "ListJobs",
            Call::CancelJob(_) => "CancelJob",
            Call::GetResult(_) => "GetResult",
            Call::GetLogs(_) => "GetLogs",
        })
        .collect()
}

/// Catches an executor built where none was asked for: a frontend of `kbf-daemon`, given or not
/// given the flag, and given every setting of a job service (as a unit is, after the rollback),
/// serves and stops without one call to the job service, and its metrics have no executor
/// family. An executor would have listed its jobs before it could be stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_runs_on_daemons_never_contacts_the_job_service() {
    for executor in [&["--executor=kbf-daemon"][..], &[]] {
        let s3 = FakeS3::start().await;
        let dir = scratch_dir("executor-default");
        let fake = job_server(&["host-1"]).await;
        let flags = flags(dir.path(), &s3, &fake, executor);
        let (handle, stop) = started(&flags, Some(source())).expect("starts");
        let api = handle.api.expect("the operator API");
        let served = tokio::spawn(handle.serve());
        let text = scrape(api).await;
        assert!(!text.contains("kbf_executor_"), "{text}");
        stop.send(()).expect("stopped");
        served.await.expect("joined").expect("served");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // The flag's directory exists and holds the release's bundle: nothing is read from it.
        assert!(s3.keys().is_empty(), "{:?}", s3.keys());
    }
}

/// Catches an executor that does not live with the frontend: it asks nothing of the job service
/// until the frontend serves, then lists the jobs an earlier process left under its id and
/// registers its worker with the farm (which lists it as a node), says so on `GET /metrics`,
/// and has nothing of its own left on the job service once the frontend has stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_js_agent_registers_its_workers_while_it_serves_and_leaves_no_job() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-js");
    let fake = job_server(&["host-1"]).await;
    let flags = flags(
        dir.path(),
        &s3,
        &fake,
        &["--executor=js-agent", "--executor-insecure-argv-secrets"],
    );
    let (handle, stop) = started(&flags, Some(source())).expect("starts");
    let api = handle.api.expect("the operator API");
    assert!(
        fake.calls().is_empty(),
        "nothing is asked until the frontend serves"
    );
    let served = tokio::spawn(handle.serve());

    let nodes = registered(api, "js:front-1-0").await;
    assert!(nodes.contains("js:front-1-0"), "{nodes}");
    let text = scrape(api).await;
    for family in [
        "kbf_executor_runs_total{outcome=\"ran\"} 0\n",
        "kbf_executor_ingests_total{result=\"ok\"} 0\n",
        "kbf_executor_late_starts_total 0\n",
        "kbf_executor_jobs_submitted_total 0\n",
        "kbf_executor_jobs_adopted_cancelled_total 0\n",
    ] {
        assert!(text.contains(family), "{family} in {text}");
    }
    // The jobs of the frontend's id were listed for adoption (the rival check's listing, of
    // every live job, has no label), and no action was placed.
    let listed = fake.calls().into_iter().find_map(|call| match call {
        Call::ListJobs(request) if !request.labels.is_empty() => Some(request),
        _ => None,
    });
    let listed: ListJobsRequest = listed.expect("the earlier jobs were listed");
    assert_eq!(
        listed.labels.get("kbf-frontend").map(String::as_str),
        Some("front-1")
    );
    assert!(
        !methods(&fake).contains(&"SubmitJob"),
        "{:?}",
        methods(&fake)
    );

    stop.send(()).expect("stopped");
    served.await.expect("joined").expect("served");
    assert!(fake.jobs().is_empty(), "{:?}", fake.jobs());
    assert!(
        !methods(&fake).contains(&"CancelJob"),
        "{:?}",
        methods(&fake)
    );
}

/// Catches two frontends that both run workers: a live job of another frontend on the job
/// service stops this frontend with an error naming it, before it has registered a worker or
/// submitted a job of its own, and the other frontend's job is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_stops_when_another_frontend_has_jobs() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-rival");
    let fake = job_server(&["host-1", "host-2"]).await;
    let theirs = JobSpec {
        job_id: "kother-1-1-1".to_owned(),
        image: "registry.test/kbf@sha256:fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
            .to_owned(),
        wall_limit_seconds: Some(3_600),
        labels: [
            ("kbf-frontend".to_owned(), "front-2".to_owned()),
            ("kbf-instance".to_owned(), "other-process".to_owned()),
        ]
        .into(),
        ..JobSpec::default()
    };
    fake.record(theirs).expect("recorded");
    let flags = flags(
        dir.path(),
        &s3,
        &fake,
        &["--executor=js-agent", "--executor-insecure-argv-secrets"],
    );
    let (handle, _stop) = started(&flags, Some(source())).expect("starts");
    let api = handle.api.expect("the operator API");
    // A frontend that does not stop would register a worker of its own: the test fails then,
    // with the nodes, instead of waiting for a stop that does not come.
    let served = tokio::select! {
        served = handle.serve() => served,
        nodes = registered(api, "js:front-1-0") => panic!("it registered a worker: {nodes}"),
    };
    let Err(ServeError::Executor(why)) = served else {
        panic!("not refused: {served:?}");
    };
    assert!(why.contains("front-2"), "{why}");
    let jobs = fake.jobs();
    assert_eq!(jobs.len(), 1, "no job of its own: {jobs:?}");
    assert_eq!(jobs[0].id, "kother-1-1-1");
    assert_eq!(jobs[0].state(), JobState::Queued, "left alone");
    assert!(!jobs[0].cancel_requested);
    assert!(
        !methods(&fake).contains(&"SubmitJob"),
        "{:?}",
        methods(&fake)
    );
}

/// Asserts that a frontend given only `--task-secret-file` and `--task-out-mode put-dir` starts
/// its executor over a preparation of put directories alone, which is the frontend's own (no
/// test source), and holds no preparation of presigned writes; and that the secret file is
/// still read at the start: a short one stops it, naming the flag. No read listener is given.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_dir_frontend_starts_over_its_own_put_dir_preparation() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-put-dir-only");
    let fake = job_server(&["host-1"]).await;
    let mut list = flag_list(
        dir.path(),
        &s3,
        &fake,
        &["--executor=js-agent", "--executor-insecure-argv-secrets"],
    );
    list.retain(|flag| {
        ![
            "--task-put-endpoint",
            "--task-put-bucket",
            "--task-put-credentials-file",
            "--task-read-url",
            "--job-",
            "--executor-allow-plain-http-put",
        ]
        .iter()
        .any(|name| flag.starts_with(name))
    });
    list.extend([
        "--task-out-mode=put-dir".to_owned(),
        "--task-out-root=/mnt/ts/put".to_owned(),
    ]);
    let flags = flags_over(Some("--storage=http://127.0.0.1:1"), &list);
    assert!(flags.task.is_none() && flags.put_dir.is_some());
    let (handle, _stop) = started(&flags, None).expect("starts over its own preparation");
    assert!(handle.put_dir_prep().is_some());
    assert!(handle.run_prep().is_none());
    drop(handle);

    std::fs::write(dir.path().join("task.secret"), b"too short\n").expect("written");
    let why = started(&flags, None).err().expect("refused").to_string();
    assert!(why.contains("--task-secret-file"), "{why}");
}

/// Catches a frontend that starts an executor it cannot run: `js-agent` without the
/// preparation of runs, with its pack flags removed after they were parsed, or with a job
/// service token file that others can read or that is missing, is refused at the start, before
/// anything is bound or asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_cannot_build_its_executor_does_not_start() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-refused");
    let fake = job_server(&["host-1"]).await;
    let js = ["--executor=js-agent", "--executor-insecure-argv-secrets"];

    let good = flags(dir.path(), &s3, &fake, &js);
    let mut no_task = good.clone();
    no_task.task = None;
    let why = started(&no_task, None).err().expect("refused").to_string();
    assert!(why.contains("preparation of task runs"), "{why}");
    assert!(why.contains("--task-secret-file"), "{why}");
    // The frontend makes its own source from the task flags: no test source is needed.
    let (own, _stop) = started(&good, None).expect("starts over its own preparation");
    drop(own);

    let mut no_packs = good.clone();
    no_packs.packs = None;
    let why = started(&no_packs, Some(source()))
        .err()
        .expect("refused")
        .to_string();
    assert!(why.contains("--pack-store-endpoint"), "{why}");

    let token = dir.path().join("job-token");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).expect("mode");
    let why = started(&good, Some(source()))
        .err()
        .expect("refused")
        .to_string();
    assert!(why.contains("--executor-job-token-file"), "{why}");

    std::fs::remove_file(&token).expect("removed");
    let why = started(&good, Some(source()))
        .err()
        .expect("refused")
        .to_string();
    assert!(why.contains("--executor-job-token-file"), "{why}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

/// Asserts that a `js-agent` frontend writes the release's base tree to the pack store and the
/// `job-tree/<root>` row to its state store while it starts to serve, before its executor runs:
/// nothing is written when the frontend is started, every object of the bundle is under `cas/l/`
/// by the time the executor has registered its worker, and the row names this build's commit and a
/// publish time. Catches: a tree published after the executor took jobs, or never, and a record
/// that is missing or names another release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_js_agent_frontend_publishes_the_base_tree_and_records_it_before_it_serves() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-base-tree");
    let fake = job_server(&["host-1"]).await;
    let flags = flags(
        dir.path(),
        &s3,
        &fake,
        &["--executor=js-agent", "--executor-insecure-argv-secrets"],
    );
    let bundled = bundle_dir(
        &dir.path().join("base-tree"),
        crate::BUILD_COMMIT_FULL,
        &["linux-x86_64"],
    );
    let (store, state) = state_store();
    let (handle, stop) = started_over(&flags, Some(source()), Some(state)).expect("starts");
    let api = handle.api.expect("the operator API");
    assert!(
        large_keys(&s3).is_empty(),
        "nothing is written by the start"
    );
    assert!(store.scan_prefix("job-tree/").expect("a scan").is_empty());
    let served = tokio::spawn(handle.serve());

    registered(api, "js:front-1-0").await;

    let held = large_keys(&s3);
    for object in &bundled.objects {
        assert!(
            held.contains(&format!("cas/l/{object}")),
            "{object} in {held:?}"
        );
    }
    let row = store
        .get(&format!("job-tree/{}", bundled.root))
        .expect("a read")
        .expect("the row");
    let row: serde_json::Value = serde_json::from_slice(&row.value).expect("JSON");
    assert_eq!(row["release"], crate::BUILD_COMMIT_FULL);
    assert!(
        row["first_published_ms"].as_u64().is_some_and(|ms| ms > 0),
        "{row}"
    );
    stop.send(()).expect("stopped");
    served.await.expect("joined").expect("served");
}

/// Asserts that an `--executor-arch` the release has no bundle for fails the start naming
/// `--executor-base-tree` and the bundle it looked for, before the executor asked the job service
/// anything, and writes no object and no row. Catches: an arm64 farm started over an x86 bundle,
/// or one that starts with no base tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_bundle_for_the_configured_arch_fails_the_start_naming_the_flag() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-base-tree-arch");
    let fake = job_server(&["host-1"]).await;
    let flags = flags(
        dir.path(),
        &s3,
        &fake,
        &[
            "--executor=js-agent",
            "--executor-insecure-argv-secrets",
            "--executor-arch=arm64",
        ],
    );
    let (store, state) = state_store();
    let (handle, _stop) = started_over(&flags, Some(source()), Some(state)).expect("starts");

    let served = handle.serve().await;

    let said = served.expect_err("refused").to_string();
    assert!(said.starts_with("--executor-base-tree: "), "{said}");
    let wanted = kbf_tree_bundle::file_name(crate::BUILD_COMMIT_FULL, "linux-arm64");
    assert!(said.contains(&wanted), "{said}");
    assert!(large_keys(&s3).is_empty());
    assert!(store.scan_prefix("job-tree/").expect("a scan").is_empty());
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

/// Asserts that a publish the store fails fails the start naming the flag and the key, and the
/// executor is never started: once with an object of the tree already in the store at another
/// length, once with the store failing the first length check. Catches: a frontend that serves
/// over a base tree it could not write, and a failure named by no key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publish_that_fails_fails_the_start() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-base-tree-store");
    let fake = job_server(&["host-1"]).await;
    let js = ["--executor=js-agent", "--executor-insecure-argv-secrets"];
    let flags = flags(dir.path(), &s3, &fake, &js);
    let bundled = bundle_dir(
        &dir.path().join("base-tree"),
        crate::BUILD_COMMIT_FULL,
        &["linux-x86_64"],
    );
    let (store, state) = state_store();
    let taken = format!("cas/l/{}", bundled.root);
    let objects = s3.store(kbf_objstore::Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    let key = kbf_objstore::ObjectKey::new(taken.clone()).expect("a key");
    kbf_objstore::ObjectStore::put_new(&objects, &key, bytes::Bytes::from_static(b"x"), None)
        .await
        .expect("a stray object");
    let (handle, _stop) = started_over(&flags, Some(source()), Some(state)).expect("starts");

    let said = handle.serve().await.expect_err("refused").to_string();

    assert!(said.starts_with("--executor-base-tree: "), "{said}");
    assert!(said.contains(&taken), "{said}");
    assert!(store.scan_prefix("job-tree/").expect("a scan").is_empty());
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());

    let dir = scratch_dir("executor-base-tree-head");
    let flags = self::flags(dir.path(), &s3, &fake, &js);
    s3.fail(Failure::Head(1));
    let (handle, _stop) = started(&flags, Some(source())).expect("starts");

    let said = handle.serve().await.expect_err("refused").to_string();

    assert!(said.starts_with("--executor-base-tree: "), "{said}");
    assert!(said.contains("length check"), "{said}");
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}
