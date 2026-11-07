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
use kbf_objstore::s3::fake::FakeS3;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use super::pack_tests::{flags_of, index, pack_flags, scrape};
use super::{FrontendFlags, FrontendHandle, Prepared, boxed};
use crate::executor::{PrepareError, PreparedRun, RunPrepare, RunSource};
use crate::serve::ServeError;
use crate::test_dir::scratch_dir;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

/// A preparation that is never asked: no action is placed in these tests.
struct Idle;

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

fn source() -> RunSource {
    RunSource {
        mint: PackKeyMint::new(SECRET).expect("a mint"),
        prepare: Arc::new(Idle),
    }
}

/// A job service with the hosts `hosts`, each a machine that fits every job the executor
/// submits.
async fn job_server(hosts: &[&str]) -> FakeJobServer {
    let fake = FakeJobServer::start().await.expect("a job service");
    for host in hosts {
        fake.add_host(host, machine());
    }
    fake
}

/// A Linux host of 16 cores, 64 GiB and 1 TiB of disk, running containers.
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
    let token = dir.join("job-token");
    std::fs::write(&token, TOKEN).expect("written");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let mut more = pack_flags(s3.endpoint(), dir);
    more.extend(beside_flags(dir));
    more.extend([
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
    flags_of(&more)
}

/// Starts the frontend `flags` describe, which stops when `stop` is sent to.
fn started(
    flags: &FrontendFlags,
    source: Option<RunSource>,
) -> Result<(FrontendHandle, oneshot::Sender<()>), Box<dyn Error>> {
    let (stop, stopped) = oneshot::channel::<()>();
    let shutdown = boxed(move || {
        Ok(async move {
            let _ = stopped.await;
        })
    });
    let handle = Prepared::new(flags)?.start(None, Some(index()), shutdown, source)?;
    Ok((handle, stop))
}

/// The whole answer of `GET path` on the operator listener at `api`, or `None` when the
/// listener refused, reset or closed the connection before it answered.
async fn try_get(api: std::net::SocketAddr, path: &str) -> Option<String> {
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
async fn registered(api: std::net::SocketAddr, worker: &str) -> String {
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
fn methods(fake: &FakeJobServer) -> Vec<&'static str> {
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
