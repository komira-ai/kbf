//! `kbf-deploy job-service apply --apply` (the cutover of a frontend onto the job service,
//! with its canary) against the real job service and the real host agent: the
//! `js-lease-server` and the `js-agent` of the jobscheduler release that `kbf-js-version.toml`
//! pins (`tests/js_real_support` says how each is fetched, checked and started).
//!
//! The verb runs as the binary runs it (`kbf_deploy::cli::run_at`), with the frontend's host
//! scripted: `kbf --check`, `kbf cluster drain` and the `ssh` to `kbf-host-install` answer as a
//! healthy host does, and the frontend's `/healthz`, `/readyz` and first-worker URLs answer as
//! a frontend that exits after the drain and comes back. What is real is everything the
//! cutover says to the job service: the check (`ListJobs` with the client token), and the
//! canary job (`SubmitJob`, `GetJob` until it ends, `CancelJob` when it does not), which the
//! service places on the agent, whose container (the stand-in `crun`) exits as the test plans
//! it (`plan/no-action`: the canary names no action).
//!
//! - `the_cutover_canary_runs_on_the_real_service_and_agent_and_passes`: the canary runs to
//!   `SUCCEEDED`, and the cutover passes.
//! - `a_canary_that_exits_non_zero_fails_the_cutover_after_the_switch`: the canary's container
//!   exits 3, the job ends `FAILED` with that exit code, and the cutover fails, saying so.
//! - `a_canary_that_does_not_end_is_cancelled_and_fails_the_cutover`: the canary's container
//!   starts and holds; after its asks the cutover cancels it, the agent stops the container, and
//!   the cutover fails.
//!
//! The cases need root (the agent runs jobs only as root), systemd (each agent runs in a scope
//! of its own) and the fetched release, so each is `#[ignore]`d with that reason: beta.yml's
//! `js-real-service` job fetches and verifies the pinned release and runs them with
//! `cargo xtask sudo-test -p kbf-integration-test --test cutover_real_service_integration`.
//! Without the release, root or sudo each fails, naming what is missing; none is skipped.
//!
//! The tests wait on what happens (a container starts, a job reaches a state, the verb
//! returns), never on time. The verb's own pause between two asks is its `--poll-secs`.

mod js_real_support;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::process::{ExitCode, Output};
use std::time::{Duration, SystemTime};

use js_proto::v1::{Job, JobState, ResultReason};
use kbf_host::testkit::reply;
use kbf_host::{Cmd, Entry, Exec, HostPath, HttpUrl, Meta, Step, Untrusted};
use kbf_integration_test::js_stand_in::NO_ACTION;
use kbf_testkit::dir::TestDir;

use js_real_support::agent::{Event, Node, RealAgent, image};
use js_real_support::{RealJobServer, TOKEN};

/// The instant the verb runs at: the canary job is `kbf-canary-<these seconds>`.
const NOW: u64 = 1_767_225_600;
const CANARY: &str = "kbf-canary-1767225600";
/// The frontend id the canary job is labelled with.
const FRONTEND: &str = "fe-a";

/// The frontend's host as the cutover reaches it: every tool answers as on a healthy host
/// (`kbf-host-install` says `restarted`), and each URL answers from its queue, the last
/// answer repeating: the frontend is up until the drain, then gone; then not ready, then
/// ready; its first worker is not there, then there. No step is applied through it: the
/// cutover edits the frontend's `kbf.toml` itself.
struct FrontendHost {
    statuses: std::sync::Mutex<BTreeMap<String, VecDeque<io::Result<u16>>>>,
}

impl FrontendHost {
    fn healthy() -> FrontendHost {
        let gone = || Err(io::ErrorKind::ConnectionRefused.into());
        let statuses = [
            ("/healthz", vec![Ok(200), gone()]),
            ("/readyz", vec![Ok(503), Ok(200)]),
            (
                &*format!("/v1/nodes/js:{FRONTEND}-0"),
                vec![Ok(404), Ok(200)],
            ),
        ]
        .into_iter()
        .map(|(path, answers)| (path.to_owned(), answers.into()))
        .collect();
        FrontendHost {
            statuses: std::sync::Mutex::new(statuses),
        }
    }
}

impl Exec for FrontendHost {
    fn euid(&self) -> u32 {
        0
    }

    fn inspect(&self, _path: &HostPath) -> io::Result<Option<Entry>> {
        Ok(None)
    }

    fn stat(&self, _path: &HostPath) -> io::Result<Option<Meta>> {
        Ok(None)
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        Ok(if cmd.program.as_str() == "/usr/bin/ssh" {
            reply(
                0,
                "{\"result\":\"restarted\",\"unit\":\"kbf-frontend.service\"}\n",
                "",
            )
        } else {
            reply(0, "", "")
        })
    }

    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        let mut statuses = self
            .statuses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queue = statuses
            .get_mut(url.path())
            .unwrap_or_else(|| panic!("the cutover asked an unscripted URL {}", url.path()));
        if queue.len() > 1 {
            return queue.pop_front().expect("a queued answer");
        }
        match &queue[0] {
            Ok(code) => Ok(*code),
            Err(e) => Err(e.kind().into()),
        }
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        panic!("the cutover applies no host step: {step:?}")
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        Ok(())
    }
}

/// The real server, an agent on a node, and the frontend's files.
struct Rig {
    dir: TestDir,
    // Dropped in this order: the agent before its node, both before the server.
    agent: RealAgent,
    node: Node,
    server: RealJobServer,
}

/// The frontend's `kbf.toml` before the cutover: ready for the switch, its kbf image the one
/// the node's image store holds, so the agent can run the canary.
fn ready_config() -> String {
    format!(
        "[scheduler]\nstate-store = \"postgres:///kbf\"\n\n[executor]\n\
         executor = \"kbf-daemon\"\nexecutor-frontend-id = \"{FRONTEND}\"\n\
         executor-base-image = \"registry.example/base@sha256:{base}\"\n\
         executor-kbf-image = \"docker://{kbf}\"\n\
         executor-secret-delivery = \"files\"\nexecutor-host-slots = 8\n\n[packs]\n\
         pack-read = true\n",
        base = "ff".repeat(32),
        kbf = image(),
    )
}

impl Rig {
    /// A rig named `name`: a placing server, a node and its agent, and a scratch directory
    /// with the frontend's `kbf.toml` and kbf's client token.
    async fn start(name: &str) -> Rig {
        let server = RealJobServer::start_placing(&format!("{name}-server")).await;
        let node = Node::new(name).await;
        let agent = RealAgent::start(&node, &server).await;
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("{name}-frontend"));
        std::fs::write(dir.path().join("kbf.toml"), ready_config()).expect("kbf.toml");
        let mut token = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.path().join("job-service.token"))
            .expect("the token file");
        writeln!(token, "{TOKEN}").expect("the token");
        Rig {
            dir,
            agent,
            node,
            server,
        }
    }

    /// `kbf-deploy job-service apply --apply` of this frontend onto the server, asking
    /// after the canary every second and cancelling it after `canary_secs`: the exit code,
    /// and stdout and stderr together.
    async fn cutover(&self, canary_secs: u64) -> (ExitCode, String) {
        let dir = self.dir.path();
        let path = |name: &str| dir.join(name).display().to_string();
        let args: Vec<OsString> = [
            "kbf-deploy",
            "job-service",
            "apply",
            "--config",
            &path("kbf.toml"),
            "--url",
            &format!("http://{}", self.server.job_addr()),
            "--token-file",
            &path("job-service.token"),
            "--hosts",
            "1",
            "--host-cpus",
            "2",
            "--host-memory-gib",
            "4",
            "--instance",
            "-",
            "--api-token-file",
            "/etc/kbf/api-token",
            "--ssh-host",
            "fe.farm.example",
            "--ssh-user",
            "kbf-updater",
            "--ssh-key-file",
            "/etc/kbf/key",
            "--known-hosts",
            "/etc/kbf/known_hosts",
            "--poll-secs",
            "1",
            "--canary-deadline-secs",
            &canary_secs.to_string(),
            "--apply",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        // The verb runs its own runtime and pauses its thread between asks: a blocking
        // thread of its own, so the test's runtime goes on meanwhile.
        tokio::task::spawn_blocking(move || {
            let mut host = FrontendHost::healthy();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
            let code = kbf_deploy::cli::run_at(args, &mut host, &|_| None, now, &mut out, &mut err);
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
            (code, said)
        })
        .await
        .expect("the verb's thread")
    }

    /// The canary job as the service holds it.
    async fn canary(&self) -> Job {
        self.server
            .client()
            .get_job(CANARY)
            .await
            .expect("GetJob of the canary")
    }

    /// The canary container's start: its arguments after the agent's init.
    async fn canary_started(&self) -> Vec<String> {
        match self.node.next().await {
            Event::Started { args, .. } => args,
            other => panic!("the canary's container started first, not {other:?}"),
        }
    }

    /// Asks the job service until the canary is in `state`, failing at once when it ends in
    /// another.
    async fn until_canary(&self, state: JobState) -> Job {
        let mut pace = tokio::time::interval(Duration::from_millis(250));
        loop {
            let job = self.canary().await;
            let now = JobState::try_from(job.state).unwrap_or(JobState::Unspecified);
            if now == state {
                return job;
            }
            assert!(
                matches!(
                    now,
                    JobState::Queued | JobState::Placed | JobState::Starting | JobState::Running
                ),
                "the canary is {now:?}, not {state:?}, and never will be: {job:?}\n{}",
                self.logs()
            );
            pace.tick().await;
        }
    }

    /// The frontend's `kbf.toml` now.
    fn config(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("kbf.toml")).expect("kbf.toml")
    }

    /// What the server, the agent and the stand-ins said, for a failure's message.
    fn logs(&self) -> String {
        format!(
            "server:\n{}\nagent:\n{}\nstand-in calls:\n{}",
            self.server.log(),
            self.agent.log(),
            self.node.calls()
        )
    }
}

/// Asserts the cutover's whole talk with the current job service on a run that passes: the
/// check takes kbf's token, the canary is submitted as the executor's kind of job (the
/// frontend's kbf image, `--version` alone, the `kbf-canary` label and no `kbf-frontend`
/// one), the service places it on the agent, whose container starts with those arguments and
/// exits 0, the service records it `SUCCEEDED` with exit code 0, and the cutover prints the
/// pass line naming the host, exits 0 and leaves the frontend switched to the job service.
/// Catches a canary the current service refuses, never places, or ends otherwise than the
/// cutover reads it, and a lease-API call left in the path (the service has none).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn the_cutover_canary_runs_on_the_real_service_and_agent_and_passes() {
    let rig = Rig::start("cut-ok").await;
    let (code, said) = rig.cutover(300).await;
    let args = rig.canary_started().await;
    assert_eq!(code, ExitCode::SUCCESS, "{said}\n{}", rig.logs());
    assert_eq!(
        &args[args.len() - 2..],
        ["/bin/task", "--version"],
        "{args:?}"
    );
    let job = rig.canary().await;
    assert_eq!(job.state(), JobState::Succeeded, "{job:?}");
    assert!(
        said.contains(&format!(
            "pass: the canary job {CANARY} succeeded on host {}\n",
            job.host_id
        )),
        "{said}"
    );
    let spec = job.spec.expect("the canary's spec");
    assert_eq!(spec.image, image());
    assert_eq!(spec.args, ["--version"]);
    assert_eq!(
        spec.labels.get("kbf-canary").map(String::as_str),
        Some(FRONTEND)
    );
    assert!(
        !spec.labels.contains_key("kbf-frontend"),
        "{:?}",
        spec.labels
    );
    let result = rig
        .server
        .client()
        .get_result(CANARY)
        .await
        .expect("GetResult");
    assert_eq!(
        (result.exit_code, result.reason()),
        (Some(0), ResultReason::Exit)
    );
    assert!(
        rig.config().contains("executor = \"js-agent\""),
        "{}",
        rig.config()
    );
}

/// Asserts a canary that fails on the real agent: its container exits 3, the service records
/// the job `FAILED` with exit code 3, and the cutover exits 1 saying the canary ended failed
/// on that host and that the switch is in place. Catches a failed canary read as a pass or as
/// still pending.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_canary_that_exits_non_zero_fails_the_cutover_after_the_switch() {
    let rig = Rig::start("cut-fail").await;
    rig.node.plan(NO_ACTION, "exit 3");
    let (code, said) = rig.cutover(300).await;
    assert_eq!(code, ExitCode::FAILURE, "{said}\n{}", rig.logs());
    let job = rig.canary().await;
    assert_eq!(job.state(), JobState::Failed, "{job:?}");
    assert!(
        said.contains(&format!(
            "the canary job {CANARY} ended failed on host {}",
            job.host_id
        )),
        "{said}"
    );
    assert!(said.contains("but a step after it failed"), "{said}");
    let result = rig
        .server
        .client()
        .get_result(CANARY)
        .await
        .expect("GetResult");
    assert_eq!(
        (result.exit_code, result.reason()),
        (Some(3), ResultReason::Exit)
    );
}

/// Asserts a canary that does not end: its container starts and holds, the cutover asks
/// after it until its deadline, then cancels it and exits 1 saying it did not end in time
/// and was cancelled; the agent stops the running container, the service records the job
/// `CANCELLED` with the reason cancelled, and the job, having ended, no longer shows the
/// cancel as pending. The deadline (30 asks) is long beside an agent that heartbeats four
/// times a second; a verb that returns before the container starts fails the test, naming
/// that, rather than passing on a cancel of a job that never ran. Catches a canary left
/// running on the farm, and a cancel the current service does not carry to the host.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_canary_that_does_not_end_is_cancelled_and_fails_the_cutover() {
    let rig = Rig::start("cut-hold").await;
    rig.node.plan(NO_ACTION, "hold");
    let verb = rig.cutover(30);
    tokio::pin!(verb);
    let name = tokio::select! {
        event = rig.node.next() => match event {
            Event::Started { name, .. } => name,
            other => panic!("the canary's container started first, not {other:?}"),
        },
        (code, said) = &mut verb => panic!(
            "the cutover returned ({code:?}) before the canary's container started: {said}\n{}",
            rig.logs()
        ),
    };
    let (code, said) = verb.await;
    assert_eq!(code, ExitCode::FAILURE, "{said}\n{}", rig.logs());
    assert!(
        said.contains(&format!(
            "the canary job {CANARY} did not end in time (still running); it was cancelled"
        )),
        "{said}"
    );
    assert_eq!(rig.node.next().await, Event::Ended { name });
    let job = rig.until_canary(JobState::Cancelled).await;
    assert!(!job.cancel_requested, "{job:?}");
    let result = rig
        .server
        .client()
        .get_result(CANARY)
        .await
        .expect("GetResult");
    assert_eq!(result.reason(), ResultReason::Cancelled, "{result:?}");
}
