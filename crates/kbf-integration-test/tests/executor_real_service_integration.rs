//! The executor of kbf-server (`kbf_server::executor::Executor`, the real one, through
//! `kbf_front::Dispatch`) against the real job service and the real host agent: the
//! `js-lease-server` and the `js-agent` of the jobscheduler release that `kbf-js-version.toml`
//! pins (`tests/js_real_support` says how each is fetched, checked and started). The agent runs
//! each job's container through a stand-in `crun` (`kbf_integration_test::js_stand_in`) that
//! tells the test when it starts, with what arguments, and when it ends, and does what the test
//! planned for its action: exit with a code, or hold until it is stopped.
//!
//! What the executor reads from the outside (the action's facts and tree, the put directory's
//! mint, the pack's ingest) is faked here as in `kbf-server/tests/executor/rig.rs`: the
//! subject is what the executor and the job service say to each other, and what the service and
//! the agent make of it. A fake job server can drift from the real one; these tests run the
//! real one.
//!
//! The cases need root (the agent runs jobs only as root), systemd (each agent runs in a scope
//! of its own) and the fetched release, so each is `#[ignore]`d with that reason: beta.yml's
//! `js-real-service` job fetches and verifies the pinned release and runs them with
//! `cargo xtask sudo-test -p kbf-integration-test --test executor_real_service_integration`.
//! Without the release, root or sudo each fails, naming what is missing; none is skipped.
//!
//! - `a_job_that_exits_0_is_the_actions_result`: success, end to end.
//! - `a_job_past_its_wall_limit_is_a_timeout`: the agent stops a container that holds past
//!   the wall limit, and the executor reports the action timed out.
//! - `a_cancelled_operation_cancels_its_running_job`: `Dispatch::cancel` reaches the job
//!   service and the host.
//! - `a_lost_answer_to_a_submit_makes_one_job`: the answer to the submit is lost after the
//!   service made the job; the executor submits again under the same id, and the service holds
//!   one job.
//! - `a_whole_machine_action_is_refused_and_no_job_is_made`: a whole-machine action cannot
//!   commit into a put directory (the service gives a whole-host job no mount), so the executor
//!   answers it as the caller's and makes no job.
//!
//! One test runs everywhere, as a pull request's tests do:
//! `the_stand_in_answers_as_the_program_it_is_linked_as`, the built stand-in under the names
//! the agent runs it by.
//!
//! The tests wait on what happens (a container starts or ends, an action finishes, a job
//! reaches a state), never on time. The executor runs each operation in a task of its own; no
//! assertion reads a clock.

mod js_real_support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_proto::v1::{Job, JobState, ListJobsRequest, ResultReason};
use kbf_front::{Described, Dispatch as _, Finished, Recorder, Stage, Submission};
use kbf_job_client::Client;
use kbf_proto::reapi::Digest;
use kbf_server::executor::{
    ActionFacts, Config, Deps, Executor, Ingest, Ingested, InputTree, Inputs, InputsError, Metrics,
    MintError, OUT_TARGET, PackKey, PutDirRoots, RunId, SizeClass, TASK_IN_TREE, UntrustedLimits,
    build_id, job_id, parse_stored_name,
};
use kbf_types::ActionKey;
use tokio::sync::watch;

use js_real_support::agent::{Event, Node, RealAgent, image};
use js_real_support::drop_answer::DropFirstAnswer;
use js_real_support::{RealJobServer, TOKEN};

const FRONTEND: &str = "front-1";

/// The action tree every run is told to read its root from: 64 hex digits.
const TREE: &str = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

/// The digest of the action of `tag`: hash `[tag; 32]`, size `tag`.
fn digest_of(tag: u8) -> kbf_types::Digest {
    kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

/// The size class of every action here, a build of `root//pkg:t`: the job names no CPU and no
/// memory, so the real service books it its cold size from the class's (empty) history.
fn class() -> SizeClass {
    let out = "buck-out/v2/art/root/7a3c1e9f0b2d4c68/pkg/__t__/o".to_owned();
    SizeClass::of_action(&kbf_server::action_usage::Shape::of(
        &["cc".to_owned()],
        &[out],
    ))
}

/// The hash of the action of `tag`, as the task tool's `--action` names it.
fn hash_of(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

/// What the executor reads of an action: the node's image, a timeout per action (60 s unless
/// the test set one), and the one tree every run reads its root from.
#[derive(Default)]
struct FakeInputs {
    timeouts: Mutex<BTreeMap<String, Duration>>,
}

#[tonic::async_trait]
impl Inputs for FakeInputs {
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        let timeouts = self.timeouts.lock().unwrap_or_else(PoisonError::into_inner);
        let timeout = timeouts.get(&action.hash).copied();
        Ok(ActionFacts {
            image: image(),
            timeout: Some(timeout.unwrap_or(Duration::from_secs(60))),
            system: kbf_server::executor::System::Linux,
            size_class: class(),
        })
    }

    async fn tree(&self, _run: RunId) -> Result<String, InputsError> {
        Ok(TREE.to_owned())
    }
}

/// The frontend's read of a task's pack: every pack holds a result of exit code 0.
#[derive(Default)]
struct FakeIngest {
    ingested: Mutex<Vec<RunId>>,
    /// The put root of the node: a run's put directory is made under it when it is minted, as
    /// the job's mount source must exist when the agent starts the job.
    put: std::path::PathBuf,
}

impl FakeIngest {
    fn under(put: std::path::PathBuf) -> Self {
        Self {
            ingested: Mutex::default(),
            put,
        }
    }
}

#[tonic::async_trait]
impl Ingest for FakeIngest {
    async fn mint(
        &self,
        run: RunId,
        _action: &Digest,
        _wall: Duration,
        _tree: InputTree,
    ) -> Result<PackKey, MintError> {
        let key = PackKey::new(format!("cas/b/{}/0-0", build_id(run)));
        let dir = self.put.join(key.as_str());
        std::fs::create_dir_all(&dir).expect("the put directory");
        // The job's user writes into it.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        Ok(key)
    }

    async fn ingest(
        &self,
        run: RunId,
        _key: &PackKey,
        _action: &Digest,
        _limits: &UntrustedLimits,
    ) -> Ingested {
        self.ingested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(run);
        Ingested::Ran {
            result: Box::default(),
            cached: false,
        }
    }
}

/// The executor's handle on one action: the operation it was given and where it is.
struct Caller {
    tag: u8,
    name: String,
    stage: watch::Receiver<Stage>,
}

impl Caller {
    /// Waits until the operation is answered.
    async fn done(&self) -> Finished {
        let mut stage = self.stage.clone();
        let seen = stage
            .wait_for(|stage| matches!(stage, Stage::Done(_)))
            .await
            .expect("the operation is answered");
        match &*seen {
            Stage::Done(finished) => finished.clone(),
            other => unreachable!("waited for Done, saw {other:?}"),
        }
    }

    /// The run of the operation's first attempt.
    fn first_run(&self) -> RunId {
        RunId {
            operation: parse_stored_name(&self.name).expect("an operation name"),
            attempt: 0,
        }
    }
}

/// The real server, an agent on a node, and the executor between them.
struct Rig {
    exec: Executor,
    cfg: Config,
    inputs: Arc<FakeInputs>,
    ingest: Arc<FakeIngest>,
    metrics: Arc<Metrics>,
    /// The job service as the test reads it, not through any relay.
    api: Client,
    // Dropped in this order: the agent before its node, both before the server.
    agent: RealAgent,
    node: Node,
    server: RealJobServer,
}

impl Rig {
    /// A rig named `name`: a server, a node and its agent, and an executor over the server.
    async fn start(name: &str) -> Rig {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
            .try_init();
        let server = RealJobServer::start_placing(&format!("{name}-server")).await;
        let node = Node::new(name).await;
        let agent = RealAgent::start(&node, &server).await;
        let api = server.client();
        let mut cfg = Config::new(FRONTEND);
        // The put directories are under the node's own mount point, a root of its agent.
        let put = node.mount_point.join("put");
        cfg.task_out = Some(PutDirRoots::new(&put.display().to_string()).expect("roots"));
        // The action tree every run's layer names: a directory under the agent's trees root.
        std::fs::create_dir_all(node.mount_point.join("trees").join(TREE)).expect("the tree");
        // The job's wall limit is the action's timeout: no margin is added.
        cfg.run_margin = Duration::ZERO;
        let inputs = Arc::new(FakeInputs::default());
        let ingest = Arc::new(FakeIngest::under(put));
        let metrics = Arc::new(Metrics::default());
        let exec = Rig::executor(&cfg, server.client(), &inputs, &ingest, &metrics);
        Rig {
            exec,
            cfg,
            inputs,
            ingest,
            metrics,
            api,
            agent,
            node,
            server,
        }
    }

    fn executor(
        cfg: &Config,
        client: Client,
        inputs: &Arc<FakeInputs>,
        ingest: &Arc<FakeIngest>,
        metrics: &Arc<Metrics>,
    ) -> Executor {
        let deps = Deps {
            client,
            ingest: ingest.clone(),
            inputs: inputs.clone(),
            metrics: Arc::clone(metrics),
            store: None,
            recorder: Recorder::off(),
        };
        Executor::new(cfg.clone(), deps).expect("a valid config")
    }

    /// Hands the action of `tag` to the executor, as the REAPI front hands an Execute.
    fn submit(&self, tag: u8) -> Caller {
        self.submit_as(tag, false)
    }

    /// Hands the action of `tag`, which asks for the whole machine, to the executor.
    fn submit_whole(&self, tag: u8) -> Caller {
        self.submit_as(tag, true)
    }

    fn submit_as(&self, tag: u8, whole_machine: bool) -> Caller {
        let submission = Submission {
            key: ActionKey {
                instance: "main".to_owned(),
                action: digest_of(tag),
            },
            whole_machine,
            call: None,
            described: Described {
                arguments: vec!["/bin/run".to_owned()],
                outputs: Vec::new(),
                label: Default::default(),
            },
            evidence: Default::default(),
        };
        let ticket = self.exec.submit(submission).expect("the executor takes it");
        Caller {
            tag,
            name: ticket.name,
            stage: ticket.stage,
        }
    }

    /// The action of `tag` runs as `plan` says.
    fn plan(&self, tag: u8, plan: &str) {
        self.node.plan(&hash_of(tag), plan);
    }

    /// The action of `tag` has a timeout, and so its job a wall limit, of `timeout`.
    fn set_timeout(&self, tag: u8, timeout: Duration) {
        self.inputs
            .timeouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(hash_of(tag), timeout);
    }

    /// Waits for a container's next event, which it returns. Fails, with the action's result,
    /// if `caller`'s action finishes first.
    async fn next_event(&self, caller: &Caller) -> Event {
        tokio::select! {
            event = self.node.next() => {
                eprintln!("{event:?}");
                event
            }
            finished = caller.done() => panic!(
                "action {} finished before its container did: {finished:?}\n{}\nhosts:\n{}",
                caller.tag,
                self.logs(),
                self.server.hosts().await
            ),
        }
    }

    /// Every job of this frontend the job service holds, in the order it lists them.
    async fn jobs(&self) -> Vec<Job> {
        let filter = ListJobsRequest {
            labels: [("kbf-frontend".to_owned(), FRONTEND.to_owned())].into(),
            ..ListJobsRequest::default()
        };
        self.api.list_all_jobs(filter).await.expect("ListJobs")
    }

    /// The jobs of the action of `tag`.
    async fn jobs_of(&self, tag: u8) -> Vec<Job> {
        let action = format!("{}/{tag}", hash_of(tag));
        let mut jobs = Vec::new();
        for job in self.jobs().await {
            let spec = job.spec.clone().unwrap_or_default();
            if spec
                .args
                .windows(2)
                .any(|pair| pair[0] == "--action" && pair[1] == action)
            {
                jobs.push(job);
            }
        }
        jobs
    }

    /// Asks the job service until the job `id` is in `state`. Fails at once when the job ends
    /// in another state, or the service names a state this client does not know: a job that
    /// can no longer reach `state` is not waited for.
    async fn until_state(&self, id: &str, state: JobState) -> Job {
        let mut pace = tokio::time::interval(self.cfg.poll);
        loop {
            let job = self.api.get_job(id).await.expect("GetJob");
            let now = JobState::try_from(job.state).unwrap_or(JobState::Unspecified);
            if now == state {
                return job;
            }
            let ended = [
                JobState::Unspecified,
                JobState::Succeeded,
                JobState::Failed,
                JobState::Cancelled,
                JobState::Lost,
                JobState::FailedToStart,
            ];
            assert!(
                !ended.contains(&now),
                "job {id} is {} ({}), not {state:?} and never will be: {job:?}\n{}",
                job.state,
                now.as_str_name(),
                self.logs()
            );
            pace.tick().await;
        }
    }

    /// The value of the executor's counter `name`.
    fn counter(&self, name: &str) -> u64 {
        let mut text = String::new();
        self.metrics.render(&mut text);
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or_else(|| panic!("no counter {name} in\n{text}"))
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

/// The value after `flag` in `args`.
fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Asserts the whole way of an action that succeeds: the executor submits it as a job of the
/// real service under the id made from the frontend and the operation's first run, the service
/// places it on the agent, the agent starts its container with the task tool's arguments (the
/// tree, the action, the run), the container exits 0, the service records the job succeeded
/// with exit code 0, the executor reads that, ingests the pack once and answers the operation
/// with the action's result. Catches a job the real service refuses, never places or ends
/// otherwise than the fake does, and an executor that reads a real ending as another.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_job_that_exits_0_is_the_actions_result() {
    let rig = Rig::start("xrs-ok").await;
    let caller = rig.submit(1);
    let run = caller.first_run();
    let Event::Started { name, args } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    assert_eq!(
        after(&args, "--action"),
        Some(format!("{}/1", hash_of(1)).as_str())
    );
    // The agent's init, then the program of the base tree and the job's arguments.
    assert_eq!(after(&args, "--"), Some(TASK_IN_TREE), "{args:?}");
    assert!(args.iter().any(|arg| arg == "--tree"), "{args:?}");
    assert_eq!(after(&args, "--lease"), Some(build_id(run).as_str()));
    // The container ends by itself.
    assert_eq!(rig.node.next().await, Event::Ended { name });
    let finished = caller.done().await;
    assert!(
        matches!(finished, Finished::Ran(_)),
        "{finished:?}\n{}",
        rig.logs()
    );

    let jobs = rig.jobs_of(1).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = &jobs[0];
    assert_eq!(job.id, job_id(FRONTEND, run));
    assert_eq!(job.state(), JobState::Succeeded);
    let result = rig.api.get_result(&job.id).await.expect("GetResult");
    assert_eq!(
        (result.exit_code, result.reason()),
        (Some(0), ResultReason::Exit)
    );
    assert_eq!(*rig.ingest.ingested.lock().expect("ingested"), [run]);
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    let spec = job.spec.clone().expect("the job's spec");
    assert_eq!(spec.image, "");
    assert_eq!(spec.command, TASK_IN_TREE);
    assert_eq!((spec.layers.len(), spec.mounts.len()), (1, 1));
    assert_eq!(spec.mounts[0].target, OUT_TARGET);
    assert_eq!(spec.wall_limit_seconds, Some(60));
}

/// Asserts that an action whose container holds past its timeout is stopped by the agent at
/// the job's wall limit (the action's timeout of 1 s: the executor adds no margin here), that
/// the service records the job failed at its wall limit, and that the executor reports the
/// action timed out (`DEADLINE_EXCEEDED`), not as an infrastructure failure. The container
/// never ends by itself, so the only way out is the limit. Catches a wall limit the real
/// service names in a way the executor does not read.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_job_past_its_wall_limit_is_a_timeout() {
    let rig = Rig::start("xrs-wall").await;
    rig.plan(2, "hold");
    rig.set_timeout(2, Duration::from_secs(1));
    let caller = rig.submit(2);
    let Event::Started { name, .. } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    // The agent stops it at the limit.
    assert_eq!(rig.node.next().await, Event::Ended { name });
    let finished = caller.done().await;
    match &finished {
        Finished::TimedOut { status, .. } | Finished::Failed(status) => {
            assert_eq!(
                tonic::Code::from_i32(status.code),
                tonic::Code::DeadlineExceeded,
                "{finished:?}\n{}",
                rig.logs()
            );
        }
        other => panic!("not a timeout: {other:?}\n{}", rig.logs()),
    }
    let jobs = rig.jobs_of(2).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].state(), JobState::Failed);
    let result = rig.api.get_result(&jobs[0].id).await.expect("GetResult");
    assert_eq!(result.reason(), ResultReason::WallLimit);
    assert_eq!(result.exit_code, None);
    assert!(rig.ingest.ingested.lock().expect("ingested").is_empty());
}

/// Asserts that cancelling an operation whose action runs cancels its job: `Dispatch::cancel`
/// answers true, the executor sends the cancel to the service, the agent stops the container
/// (the stand-in `crun` is asked to kill it, and it ends), the service records the job
/// cancelled, for the client's cancel, and the operation's callers are answered `CANCELLED`.
/// Catches a cancel the real service takes but never carries to the host, or answers in a way
/// the executor takes for a failure to cancel.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_cancelled_operation_cancels_its_running_job() {
    let rig = Rig::start("xrs-cancel").await;
    rig.plan(3, "hold");
    let caller = rig.submit(3);
    let Event::Started { name, .. } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    let jobs = rig.jobs_of(3).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let id = jobs[0].id.clone();
    rig.until_state(&id, JobState::Running).await;
    assert!(rig.exec.cancel(&caller.name));
    assert_eq!(rig.node.next().await, Event::Ended { name: name.clone() });
    let job = rig.until_state(&id, JobState::Cancelled).await;
    // Ended, the job no longer shows the cancel as pending (`job.proto`); the result's reason
    // says it was the client's.
    assert!(!job.cancel_requested, "{job:?}");
    let result = rig.api.get_result(&id).await.expect("GetResult");
    assert_eq!(result.reason(), ResultReason::Cancelled);
    assert_eq!(rig.counter("kbf_executor_jobs_cancelled_total"), 1);
    assert!(
        rig.node.calls().contains(&format!(" kill {name} 15\n")),
        "{}",
        rig.logs()
    );
    match caller.done().await {
        Finished::Failed(status) => assert_eq!(
            tonic::Code::from_i32(status.code),
            tonic::Code::Cancelled,
            "{status:?}"
        ),
        other => panic!("not cancelled: {other:?}\n{}", rig.logs()),
    }
}

/// Asserts that a submit whose answer is lost after the service made the job is sent again
/// with the same job id, that the real service answers the repeat from the job it holds (one
/// job of the action, not two), and that the action then runs to its end as usual. The relay
/// in front of the service loses the first answer it gives once armed, which is the submit's:
/// nothing else asks the service before the Execute. Catches a resubmit under a fresh id (a
/// second job, run twice), and a real service that refuses the repeat of a job it holds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_lost_answer_to_a_submit_makes_one_job() {
    let mut rig = Rig::start("xrs-lost").await;
    let relay = DropFirstAnswer::held(rig.server.job_addr()).await;
    let client = RealJobServer::client_at(&relay.origin(), TOKEN);
    rig.exec = Rig::executor(&rig.cfg, client, &rig.inputs, &rig.ingest, &rig.metrics);
    relay.arm();
    let caller = rig.submit(4);
    let finished = caller.done().await;
    assert!(
        matches!(finished, Finished::Ran(_)),
        "{finished:?}\n{}",
        rig.logs()
    );
    assert!(relay.dropped(), "no answer was lost");
    let jobs = rig.jobs_of(4).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].state(), JobState::Succeeded);
    assert_eq!(jobs[0].id, job_id(FRONTEND, caller.first_run()));
    assert_eq!(
        rig.counter("kbf_executor_jobs_resubmitted_after_lost_answer_total"),
        1
    );
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    assert_eq!(rig.ingest.ingested.lock().expect("ingested").len(), 1);
}

/// Asserts that a whole-machine action is refused as the caller's and makes no job: it cannot
/// commit into a put directory, the real service gives a whole-host job no mount, so the
/// executor answers the action `INVALID_ARGUMENT` before it submits anything. Catches an
/// executor that sends the real service a whole-host job with a mount.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_whole_machine_action_is_refused_and_no_job_is_made() {
    let rig = Rig::start("xrs-whole").await;
    let whole = rig.submit_whole(5);
    match whole.done().await {
        Finished::Failed(status) => assert_eq!(
            tonic::Code::from_i32(status.code),
            tonic::Code::InvalidArgument,
            "{}",
            rig.logs()
        ),
        other => panic!("not refused: {other:?}\n{}", rig.logs()),
    }
    let jobs = rig.jobs_of(5).await;
    assert!(jobs.is_empty(), "{jobs:?}");
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 0);
}
/// Asserts that the built stand-in answers as the program the name it is run by says, with the
/// place beside it: as `crun --version` it names a release the agent takes, as `ip -j link
/// show` it lists no link, both recorded in `stand-in/calls`, and under a name it does not know
/// it fails saying so. Runs without root. Catches a binary that answers as one program under
/// every name, or does not find its place beside the link it is run by.
#[test]
fn the_stand_in_answers_as_the_program_it_is_linked_as() {
    use std::process::Command;
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "xrs-stand-in");
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_kbf-js-stand-in"));
    std::fs::create_dir_all(dir.join("stand-in")).expect("the place");
    for name in ["crun", "ip", "runc"] {
        std::fs::hard_link(exe, dir.join(name)).expect("a link");
    }
    let run = |name: &str, args: &[&str]| {
        Command::new(dir.join(name))
            .args(args)
            .output()
            .expect("the stand-in runs")
    };
    let version = run("crun", &["--version"]);
    assert!(version.status.success(), "{version:?}");
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        kbf_integration_test::js_stand_in::VERSION
    );
    let links = run("ip", &["-j", "link", "show"]);
    assert_eq!(
        (links.status.code(), links.stdout.as_slice()),
        (Some(0), &b"[]"[..])
    );
    let unknown = run("runc", &["run"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&unknown.stderr),
        "kbf-js-stand-in: no program \"runc\""
    );
    let calls = std::fs::read_to_string(dir.join("stand-in/calls")).expect("calls");
    assert_eq!(calls, "crun --version\nip -j link show\nrunc run\n");
}
