//! A frontend started with the executor flags, over a lease server in this process: with no
//! executor it never contacts the lease server; with `js-agent` it holds hosts while it serves,
//! counts them on `GET /metrics`, releases them when it stops, and stops by itself when another
//! frontend holds hosts.

use std::error::Error;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use kbf_front::PackKeyMint;
use kbf_lease_client::fake::{FakeLeaseServer, Pool, TOKEN};
use kbf_lease_client::{AcquireRequest, Client, LeaseState, Resources};
use kbf_objstore::s3::fake::FakeS3;
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

/// A lease server of `hosts`, hosts that report what the executor asks for.
async fn lease_server(hosts: &[&str]) -> FakeLeaseServer {
    let mut pool = Pool::new("farm-linux", hosts, 2_000, 16 << 30);
    pool.capabilities
        .insert("arch".to_owned(), "x86_64".to_owned());
    FakeLeaseServer::start(pool).await
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
/// `executor`, which name the lease server `fake`.
fn flags(dir: &Path, s3: &FakeS3, fake: &FakeLeaseServer, executor: &[&str]) -> FrontendFlags {
    let token = dir.join("lease-token");
    std::fs::write(&token, TOKEN).expect("written");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let mut more = pack_flags(s3.endpoint(), dir);
    more.extend(beside_flags(dir));
    more.extend([
        format!("--executor-lease-url={}", fake.origin()),
        format!("--executor-lease-token-file={}", token.display()),
        "--executor-frontend-id=front-1".to_owned(),
        "--executor-class=farm-linux".to_owned(),
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

fn live(leases: &[kbf_lease_client::Lease]) -> usize {
    leases
        .iter()
        .filter(|l| matches!(l.state, LeaseState::Held | LeaseState::Pending))
        .count()
}

async fn leases_of(client: &Client) -> Vec<kbf_lease_client::Lease> {
    client.leases().await.expect("leases")
}

/// Waits until the lease server lists a lease of the frontend `id`, and returns the leases.
async fn leased_by(client: &Client, id: &str) -> Vec<kbf_lease_client::Lease> {
    loop {
        let leases = leases_of(client).await;
        if leases
            .iter()
            .any(|l| l.labels.get("kbf-frontend").is_some_and(|v| v == id))
        {
            return leases;
        }
        tokio::task::yield_now().await;
    }
}

/// Waits until `check` holds of the scraped metrics of `api`, asking again each time the
/// frontend has had the chance to do something.
async fn scraped_until(api: std::net::SocketAddr, check: impl Fn(&str) -> bool) -> String {
    let mut text = String::new();
    for _ in 0..100_000 {
        text = scrape(api).await;
        if check(&text) {
            return text;
        }
        tokio::task::yield_now().await;
    }
    panic!("the metrics never held: {text}");
}

/// Catches an executor built where none was asked for: a frontend of `kbf-daemon`, given or not
/// given the flag, and given every setting of a lease server (as a unit is, after the rollback),
/// serves and stops without one request to the lease server, and its metrics have no executor
/// family. An executor would have asked for its leases before it could be stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_runs_on_daemons_never_contacts_the_lease_server() {
    for executor in [&["--executor=kbf-daemon"][..], &[]] {
        let s3 = FakeS3::start().await;
        let dir = scratch_dir("executor-default");
        let fake = lease_server(&["node1"]).await;
        let flags = flags(dir.path(), &s3, &fake, executor);
        let (handle, stop) = started(&flags, Some(source())).expect("starts");
        let api = handle.api.expect("the operator API");
        let served = tokio::spawn(handle.serve());
        let text = scrape(api).await;
        assert!(!text.contains("kbf_executor_"), "{text}");
        stop.send(()).expect("stopped");
        served.await.expect("joined").expect("served");
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    }
}

/// Catches an executor that does not live with the frontend: it holds a host while the frontend
/// serves, says so on `GET /metrics`, and the lease is released by the time the frontend has
/// stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_js_agent_holds_a_host_while_it_serves_and_releases_it_when_it_stops() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-js");
    let fake = lease_server(&["node1"]).await;
    let api_client = fake.client().expect("a client");
    let flags = flags(
        dir.path(),
        &s3,
        &fake,
        &["--executor=js-agent", "--executor-insecure-argv-secrets"],
    );
    let (handle, stop) = started(&flags, Some(source())).expect("starts");
    let api = handle.api.expect("the operator API");
    assert!(
        fake.requests().is_empty(),
        "nothing is asked until the frontend serves"
    );
    let served = tokio::spawn(handle.serve());

    let text = scraped_until(api, |text| text.contains("kbf_executor_leases_held 1\n")).await;
    for family in [
        "kbf_executor_runs_total{outcome=\"ran\"} 0\n",
        "kbf_executor_ingests_total{result=\"ok\"} 0\n",
        "kbf_executor_late_starts_total 0\n",
        "kbf_executor_stray_leases_released_total 0\n",
    ] {
        assert!(text.contains(family), "{family} in {text}");
    }
    let held = leases_of(&api_client).await;
    assert_eq!(live(&held), 1);
    assert_eq!(held[0].labels["kbf-frontend"], "front-1");

    stop.send(()).expect("stopped");
    served.await.expect("joined").expect("served");
    assert_eq!(live(&leases_of(&api_client).await), 0, "released");
}

/// Catches two frontends that both run hosts: a lease of another frontend on the lease server
/// stops this frontend with an error naming it, before it has leased a host of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_stops_when_another_frontend_holds_hosts() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-rival");
    let fake = lease_server(&["node1", "node2"]).await;
    let api_client = fake.client().expect("a client");
    let rival = AcquireRequest::new(
        "farm-linux",
        Resources {
            cpu_millis: 2_000,
            memory_bytes: 16 << 30,
            gpus: 0,
            gpu_class: None,
        },
        std::time::Duration::from_secs(3_600),
    )
    .label("kbf-frontend", "front-2");
    api_client.acquire(&rival).await.expect("booked");
    let flags = flags(
        dir.path(),
        &s3,
        &fake,
        &["--executor=js-agent", "--executor-insecure-argv-secrets"],
    );
    let (handle, _stop) = started(&flags, Some(source())).expect("starts");
    // A frontend that does not stop would lease a host of its own: the test fails then, with
    // the leases, instead of waiting for a stop that does not come.
    let served = tokio::select! {
        served = handle.serve() => served,
        held = leased_by(&api_client, "front-1") => panic!("it leased a host of its own: {held:?}"),
    };
    let Err(ServeError::Executor(why)) = served else {
        panic!("not refused: {served:?}");
    };
    assert!(why.contains("front-2"), "{why}");
    let held = leases_of(&api_client).await;
    assert_eq!(held.len(), 1, "no lease of its own: {held:?}");
    assert_eq!(held[0].labels["kbf-frontend"], "front-2");
}

/// Catches a frontend that starts an executor it cannot run: `js-agent` without the
/// preparation of runs, with a token file others can read, or with its pack flags removed
/// after they were parsed is refused at the start, before anything is bound or asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_cannot_build_its_executor_does_not_start() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("executor-refused");
    let fake = lease_server(&["node1"]).await;
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

    std::fs::set_permissions(
        dir.path().join("lease-token"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("mode");
    let why = started(&good, Some(source()))
        .err()
        .expect("refused")
        .to_string();
    assert!(why.contains("--executor-lease-token-file"), "{why}");
    assert!(fake.requests().is_empty());
}
