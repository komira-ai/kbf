//! The host executor of kbf-server (`kbf_server::executor::Executor`, the real one, over the
//! real `Farm`) against the real job service and the real host agent: the `js-lease-server`
//! and the `js-agent` of the jobscheduler release that `kbf-js-version.toml` pins
//! (`tests/js_real_support` says how each is fetched, checked and started). The agent runs each
//! job's container through a stand-in `crun` (`kbf_integration_test::js_stand_in`) that tells
//! the test when it starts, with what arguments, and when it ends, and does what the test
//! planned for its action: exit with a code, or hold until it is stopped.
//!
//! What the executor reads from the outside (the action's facts and read grant, the presigned
//! `PUT`, the pack's ingest) is faked here as in `kbf-server/tests/executor/rig.rs`: the
//! subject is what the executor and the job service say to each other, and what the scheduler
//! and the agent make of it. A fake job server can drift from the real one; these tests run the
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
//! - `a_frontend_that_stops_cancels_its_running_job`: cancel.
//! - `a_lost_answer_to_a_submit_makes_one_job`: the answer to the submit is lost after the
//!   service made the job; the executor submits again under the same id, and the service holds
//!   one job.
//! - `a_whole_machine_action_is_never_a_job`: the executor's workers offer the container
//!   driver only, so the farm never gives one a whole-machine action, and no job is made of it.
//! - `a_restarted_frontend_cancels_the_earlier_job_and_runs_the_action_again`: restart.
//!
//! One test runs everywhere, as a pull request's tests do:
//! `the_stand_in_answers_as_the_program_it_is_linked_as`, the built stand-in under the names
//! the agent runs it by.
//!
//! The tests wait on what happens (a container starts or ends, an action finishes, a job
//! reaches a state), never on time. The executor and the farm are stepped at the executor's
//! poll interval, as the frontend's driver steps them; no assertion reads a clock.

mod js_real_support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_proto::v1::{Job, JobState, ListJobsRequest, ResultReason};
use kbf_front::{Cache, Described, Dispatch as _, Finished, Stage, Submission};
use kbf_job_client::Client;
use kbf_proto::reapi::Digest;
use kbf_sched::Request;
use kbf_server::Farm;
use kbf_server::executor::{
    ActionFacts, Config, Deps, Executor, Ingest, Ingested, Inputs, InputsError, Metrics, MintError,
    PackKey, Presign, PresignError, ReadGrant, Secret, UntrustedLimits, build_id, job_id,
};
use kbf_types::{ActionKey, LeaseKind, Qos, Resources};
use tokio::sync::watch;

use js_real_support::agent::{Event, Node, RealAgent, image};
use js_real_support::drop_answer::DropFirstAnswer;
use js_real_support::{RealJobServer, TOKEN};

const MIB: u64 = 1 << 20;
const FRONTEND: &str = "front-1";

/// The digest of the action of `tag`: hash `[tag; 32]`, size `tag`.
fn digest_of(tag: u8) -> kbf_types::Digest {
    kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

/// The hash of the action of `tag`, as the task tool's `--action` names it.
fn hash_of(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

/// What the executor reads of an action: the node's image, and a timeout per action (60 s
/// unless the test set one).
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
        })
    }

    async fn read_grant(
        &self,
        _lease: kbf_types::LeaseId,
        _action: &Digest,
        _expires: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Ok(ReadGrant {
            url: "https://grant.test:9400".to_owned(),
            token: Secret::new("read-token-0001"),
            input_bytes: Some(0),
        })
    }
}

/// A store that signs a url for any key.
struct FakePresign;

#[tonic::async_trait]
impl Presign for FakePresign {
    async fn put_url(
        &self,
        key: &PackKey,
        _expires: Duration,
        _max_bytes: u64,
    ) -> Result<Secret, PresignError> {
        Ok(Secret::new(format!("https://store.test/{key}?sig=s3cr3t")))
    }
}

/// The frontend's read of a task's pack: every pack holds a result of exit code 0.
#[derive(Default)]
struct FakeIngest {
    ingested: Mutex<Vec<kbf_types::LeaseId>>,
}

#[tonic::async_trait]
impl Ingest for FakeIngest {
    async fn mint(
        &self,
        lease: kbf_types::LeaseId,
        _action: &Digest,
        _wall: Duration,
    ) -> Result<PackKey, MintError> {
        Ok(PackKey::new(format!("cas/b/{}/0-0", build_id(lease))))
    }

    async fn ingest(
        &self,
        lease: kbf_types::LeaseId,
        _key: &PackKey,
        _action: &Digest,
        _limits: &UntrustedLimits,
    ) -> Ingested {
        self.ingested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(lease);
        Ingested::Ran {
            result: Box::default(),
            cached: false,
        }
    }
}

/// The farm's handle on one action.
struct Caller {
    tag: u8,
    stage: watch::Receiver<Stage>,
}

impl Caller {
    fn finished(&self) -> Option<Finished> {
        match &*self.stage.borrow() {
            Stage::Done(finished) => Some(finished.clone()),
            _ => None,
        }
    }
}

/// The real server, an agent on a node, a farm, and the executor between them.
struct Rig {
    exec: Executor,
    cfg: Config,
    farm: Arc<Farm>,
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
    /// A rig named `name`: a server, a node and its agent, a farm and an executor of one
    /// worker.
    async fn start(name: &str) -> Rig {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
            .try_init();
        let server = RealJobServer::start_placing(&format!("{name}-server")).await;
        let node = Node::new(name).await;
        let agent = RealAgent::start(&node, &server).await;
        let api = server.client();
        let mut cfg = Config::new(FRONTEND, 1, 2_000, 4 << 30);
        cfg.slots = 2;
        cfg.insecure_argv_secrets = true;
        // The job's wall limit is the action's timeout: no margin is added.
        cfg.run_margin = Duration::ZERO;
        // A job asks for 1 GiB of disk beside its output's, and the service wants a host whose
        // scratch has that many whole GiB free: the node's scratch is on `/run`, a tmpfs of a
        // tenth of a hosted runner's memory, which has more than 1 GiB and less than 2.
        cfg.output_disk_bytes = 0;
        let farm = Arc::new(Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ));
        let inputs = Arc::new(FakeInputs::default());
        let ingest = Arc::new(FakeIngest::default());
        let metrics = Arc::new(Metrics::default());
        let exec = Rig::executor(&cfg, &farm, server.client(), &inputs, &ingest, &metrics);
        Rig {
            exec,
            cfg,
            farm,
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
        farm: &Arc<Farm>,
        client: Client,
        inputs: &Arc<FakeInputs>,
        ingest: &Arc<FakeIngest>,
        metrics: &Arc<Metrics>,
    ) -> Executor {
        let deps = Deps {
            farm: Arc::clone(farm),
            client,
            ingest: ingest.clone(),
            presign: Arc::new(FakePresign),
            inputs: inputs.clone(),
            metrics: Arc::clone(metrics),
        };
        Executor::new(cfg.clone(), deps).expect("a valid config")
    }

    /// The executor of a restarted frontend: the same frontend id, a process of its own, over
    /// the same farm and job service.
    fn restarted(&self) -> Executor {
        let mut cfg = self.cfg.clone();
        cfg.instance = Config::new(FRONTEND, 1, 2_000, 4 << 30).instance;
        Rig::executor(
            &cfg,
            &self.farm,
            self.server.client(),
            &self.inputs,
            &self.ingest,
            &self.metrics,
        )
    }

    /// Submits the action of `tag` as a lease of `kind`, booking half a core and 256 MiB.
    fn submit(&self, tag: u8, kind: LeaseKind) -> Caller {
        let request = Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: digest_of(tag),
            }
            .into(),
            qos: Qos::Ci,
            kind,
            resources: Resources::new(500, 256 * MIB),
            hermetic: true,
            do_not_cache: false,
            whole_worker: false,
            pin: None,
            needs: Default::default(),
        };
        let submission = Submission {
            request,
            call: None,
            described: Described {
                arguments: vec!["/bin/run".to_owned()],
                outputs: Vec::new(),
                label: Default::default(),
            },
            evidence: Default::default(),
            max_wall: None,
        };
        let ticket = self.farm.submit(submission).expect("the farm takes it");
        Caller {
            tag,
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

    /// Steps the executor and the farm until `done` holds of the rig.
    async fn until(&mut self, what: &str, done: impl Fn(&Rig) -> bool) {
        let mut pace = tokio::time::interval(self.exec.poll());
        while !done(self) {
            turn(&mut self.exec, &self.farm, &mut pace).await;
        }
        eprintln!("{what}");
    }

    /// Steps the executor and the farm until a container's next event, which it returns. A
    /// pass is never cut short: the event is looked for between passes. Fails, with the
    /// action's result, if `caller`'s action finishes first.
    async fn next_event(&mut self, caller: &Caller) -> Event {
        let mut pace = tokio::time::interval(self.exec.poll());
        loop {
            if let Some(event) = self.node.try_next() {
                eprintln!("{event:?}");
                return event;
            }
            if let Some(finished) = caller.finished() {
                panic!(
                    "action {} finished before its container did: {finished:?}\n{}\nhosts:\n{}",
                    caller.tag,
                    self.logs(),
                    self.server.hosts().await
                );
            }
            turn(&mut self.exec, &self.farm, &mut pace).await;
        }
    }

    /// Steps until `caller`'s action is done.
    async fn finish(&mut self, caller: &Caller) -> Finished {
        let what = format!("action {} done", caller.tag);
        self.until(&what, |_| caller.finished().is_some()).await;
        caller.finished().expect("done")
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
        let mut pace = tokio::time::interval(self.exec.poll());
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

/// One pass of the executor and of the farm, then the executor's poll interval, as the
/// frontend's driver paces them.
async fn turn(exec: &mut Executor, farm: &Farm, pace: &mut tokio::time::Interval) {
    exec.step().await;
    farm.tick().await;
    pace.tick().await;
}

/// The value after `flag` in `args`.
fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Asserts the whole way of an action that succeeds: the executor submits it as a job of the
/// real service under the id made from the frontend and the farm lease, the scheduler places it
/// on the agent, the agent starts its container from the node's image with the task tool's
/// arguments (the action, the lease, the grant), the container exits 0, the service records
/// the job succeeded with exit code 0, the executor reads that, ingests the pack once and
/// reports the action ran. Catches a job the real service refuses, never places or ends
/// otherwise than the fake does, and an executor that reads a real ending as another.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_job_that_exits_0_is_the_actions_result() {
    let mut rig = Rig::start("xrs-ok").await;
    let caller = rig.submit(1, LeaseKind::Action);
    let Event::Started { name, args } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    assert_eq!(
        after(&args, "--action"),
        Some(format!("{}/1", hash_of(1)).as_str())
    );
    assert_eq!(after(&args, "--grant-url"), Some("https://grant.test:9400"));
    // The agent's init, then the image's entrypoint and the job's arguments.
    assert_eq!(after(&args, "--"), Some("/bin/task"), "{args:?}");
    let lease = after(&args, "--lease").expect("--lease").to_owned();
    // The container ends by itself, whether the executor is stepped or not.
    assert_eq!(rig.node.next().await, Event::Ended { name });
    let finished = rig.finish(&caller).await;
    assert!(
        matches!(finished, Finished::Ran(_)),
        "{finished:?}\n{}",
        rig.logs()
    );

    let jobs = rig.jobs_of(1).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let job = &jobs[0];
    assert!(
        job.id.ends_with(&format!("-{lease}-0")),
        "{} for lease {lease}",
        job.id
    );
    assert_eq!(job.state(), JobState::Succeeded);
    let result = rig.api.get_result(&job.id).await.expect("GetResult");
    assert_eq!(
        (result.exit_code, result.reason()),
        (Some(0), ResultReason::Exit)
    );
    assert_eq!(rig.ingest.ingested.lock().expect("ingested").len(), 1);
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    let spec = job.spec.clone().expect("the job's spec");
    assert_eq!(spec.image, image());
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
    let mut rig = Rig::start("xrs-wall").await;
    rig.plan(2, "hold");
    rig.set_timeout(2, Duration::from_secs(1));
    let caller = rig.submit(2, LeaseKind::Action);
    let Event::Started { name, .. } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    // The agent stops it at the limit, whether the executor is stepped or not.
    assert_eq!(rig.node.next().await, Event::Ended { name });
    let finished = rig.finish(&caller).await;
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

/// Asserts that a frontend that stops while its action runs cancels the job: the executor's
/// shutdown sends the cancel, the agent stops the container (the stand-in `crun` is asked to
/// kill it, and it ends), and the service records the job cancelled, for the client's cancel.
/// Catches a cancel the real service takes but never carries to the host, or answers in a way
/// the executor takes for a failure to cancel.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_frontend_that_stops_cancels_its_running_job() {
    let mut rig = Rig::start("xrs-cancel").await;
    rig.plan(3, "hold");
    let caller = rig.submit(3, LeaseKind::Action);
    let Event::Started { name, .. } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    let jobs = rig.jobs_of(3).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    let id = jobs[0].id.clone();
    rig.until_state(&id, JobState::Running).await;
    rig.exec.shutdown().await;
    assert_eq!(rig.node.next().await, Event::Ended { name: name.clone() });
    let job = rig.until_state(&id, JobState::Cancelled).await;
    assert!(job.cancel_requested, "{job:?}");
    let result = rig.api.get_result(&id).await.expect("GetResult");
    assert_eq!(result.reason(), ResultReason::Cancelled);
    assert_eq!(rig.counter("kbf_executor_jobs_cancelled_total"), 1);
    assert!(
        rig.node.calls().contains(&format!(" kill {name} 15\n")),
        "{}",
        rig.logs()
    );
    assert!(
        caller.finished().is_none(),
        "the farm keeps the action for another worker"
    );
}

/// Asserts that a submit whose answer is lost after the service made the job is sent again
/// with the same job id, that the real service answers the repeat from the job it holds (one
/// job of the action, not two), and that the action then runs to its end as usual. The relay
/// in front of the service loses the first answer after the executor's workers are up, which
/// is the submit's. Catches a resubmit under a fresh id (a second job, run twice), and a real
/// service that refuses the repeat of a job it holds.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_lost_answer_to_a_submit_makes_one_job() {
    let mut rig = Rig::start("xrs-lost").await;
    let relay = DropFirstAnswer::held(rig.server.job_addr()).await;
    let client = RealJobServer::client_at(&relay.origin(), TOKEN);
    rig.exec = Rig::executor(
        &rig.cfg,
        &rig.farm,
        client,
        &rig.inputs,
        &rig.ingest,
        &rig.metrics,
    );
    rig.until("workers up", |rig| !rig.exec.workers().is_empty())
        .await;
    relay.arm();
    let caller = rig.submit(4, LeaseKind::Action);
    let finished = rig.finish(&caller).await;
    assert!(
        matches!(finished, Finished::Ran(_)),
        "{finished:?}\n{}",
        rig.logs()
    );
    assert!(relay.dropped(), "no answer was lost");
    let jobs = rig.jobs_of(4).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].state(), JobState::Succeeded);
    assert_eq!(
        rig.counter("kbf_executor_jobs_resubmitted_after_lost_answer_total"),
        1
    );
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    assert_eq!(rig.ingest.ingested.lock().expect("ingested").len(), 1);
}

/// Asserts that a whole-machine action is never made a job: the executor's workers offer the
/// container driver only, so the farm places it on none of them, while a container action
/// submitted after it runs to its end on the same workers; the service then holds the
/// container action's job and nothing of the other. Catches an executor that sends the real
/// service a job its agents cannot run (whole-host jobs are not placed by this release).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_whole_machine_action_is_never_a_job() {
    let mut rig = Rig::start("xrs-whole").await;
    let whole = rig.submit(5, LeaseKind::WholeMachine);
    let container = rig.submit(6, LeaseKind::Action);
    let finished = rig.finish(&container).await;
    assert!(
        matches!(finished, Finished::Ran(_)),
        "{finished:?}\n{}",
        rig.logs()
    );
    let jobs = rig.jobs().await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(rig.jobs_of(6).await.len(), 1);
    assert!(rig.jobs_of(5).await.is_empty());
    assert!(whole.finished().is_none(), "{:?}", whole.stage.borrow());
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
}

/// Asserts what a frontend that restarts does with the job its earlier process left running:
/// the restarted executor (same frontend id, a process of its own) finds the job on the real
/// service by its frontend label, cancels it once no new job of the earlier process appears
/// (the agent stops its container), and the farm, whose lease the earlier process held, runs
/// the action again as a new job under a new id, which succeeds. The earlier job is not taken
/// over: tasks live in the process that made them. Catches a restarted frontend that leaves
/// the earlier job running beside the rerun, or that the real service's listing hides it from.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root, systemd and the pinned js-lease-server and js-agent: beta.yml's js-real-service job fetches them and runs these with cargo xtask sudo-test"]
async fn a_restarted_frontend_cancels_the_earlier_job_and_runs_the_action_again() {
    let mut rig = Rig::start("xrs-restart").await;
    rig.plan(7, "hold");
    let caller = rig.submit(7, LeaseKind::Action);
    let Event::Started { name: first, .. } = rig.next_event(&caller).await else {
        panic!("a start first\n{}", rig.logs());
    };
    let earlier = rig.jobs_of(7).await;
    assert_eq!(earlier.len(), 1, "{earlier:?}");
    let earlier = earlier[0].clone();
    // The earlier process ends without a word; the next one runs the action to its end.
    rig.plan(7, "exit 0");
    rig.exec = rig.restarted();
    assert_eq!(rig.next_event(&caller).await, Event::Ended { name: first });
    let job = rig.until_state(&earlier.id, JobState::Cancelled).await;
    assert!(job.cancel_requested, "{job:?}");
    assert_eq!(rig.counter("kbf_executor_jobs_adopted_cancelled_total"), 1);
    let finished = rig.finish(&caller).await;
    assert!(
        matches!(finished, Finished::Ran(_)),
        "{finished:?}\n{}",
        rig.logs()
    );
    let jobs = rig.jobs_of(7).await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    let later = jobs
        .iter()
        .find(|job| job.id != earlier.id)
        .expect("the rerun");
    assert_eq!(later.state(), JobState::Succeeded);
    let instance = |job: &Job| {
        job.spec
            .clone()
            .unwrap_or_default()
            .labels
            .get("kbf-instance")
            .cloned()
    };
    assert!(instance(later).is_some());
    assert_ne!(instance(later), instance(&earlier));
    assert_eq!(later.id, job_id(FRONTEND, lease_of(later), 0));
}

/// The farm lease the job `job` was made for, read back from its task tool's `--lease`.
fn lease_of(job: &Job) -> kbf_types::LeaseId {
    let spec = job.spec.clone().unwrap_or_default();
    let lease = after(&spec.args, "--lease").expect("--lease").to_owned();
    let (term, seq) = lease.split_once('-').expect("<term>-<seq>");
    kbf_types::LeaseId::new(term.parse().expect("a term"), seq.parse().expect("a seq"))
}

/// Asserts that the built stand-in answers as the program the name it is run by says, with the
/// place beside it: as `crun --version` it names a release the agent takes, as `ip -j link
/// show` it lists no link, both recorded in `stand-in/calls`, and under a name it does not know
/// it fails saying so. Runs without root. Catches a binary that answers as one program under
/// every name, or reads its place from somewhere the agent's environment-free calls cannot.
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
            .env_clear()
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
