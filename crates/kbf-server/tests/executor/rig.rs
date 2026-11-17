//! The harness: an executor over a fake job service, and fakes for what the executor reads from
//! the rest of the server.
//!
//! Time here is real, bounded by [`PROMPT`] wherever a test waits for an event; the executor
//! reads its jobs every few milliseconds ([`POLL`]). A test never asserts how long anything took.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use js_proto::v1::{
    ExecMode, GetJobResponse, GetResultResponse, Job, JobResult, JobSpec, JobState, ResultReason,
    Shape,
};
use kbf_auth::AuthenticationMetadata;
use kbf_front::{
    CallMetadata, Described, Dispatch, ExecuteCall, Finished, InvocationId, Recorder, Stage,
    Submission,
};
use kbf_job_client::Client;
use kbf_job_client::fake::{Call, FakeJobServer, Fault, Machine};
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_server::executor::rows::Rows;
use kbf_server::executor::{
    ActionFacts, Config, Deps, Executor, Ingest, Ingested, InputTree, Inputs, InputsError, Metrics,
    MintError, PackKey, PutDirRoots, RunId, SizeClass, System, UntrustedLimits, build_id,
};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::{ActionKey, DigestFunction};
use prost::Message as _;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(10);

/// How often the rig's executor reads its jobs back.
pub const POLL: Duration = Duration::from_millis(5);

/// The one Linux host of a rig that has any.
pub const HOST: &str = "host-1";

/// The class of a build of `root//pkg:t`, which every action of the fake inputs is:
/// `root.pkg.t.build`.
pub fn built_class() -> SizeClass {
    let out = "buck-out/v2/art/root/7a3c1e9f0b2d4c68/pkg/__t__/o".to_owned();
    let shape = kbf_server::action_usage::Shape::of(&["cc".to_owned()], &[out]);
    SizeClass::of_action(&shape)
}

pub const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the executor reads of an action, and the tree of its put-directory run.
pub struct FakeInputs {
    pub facts: Mutex<Result<ActionFacts, InputsError>>,
    /// The action tree of a put-directory run.
    pub tree: Mutex<Result<String, InputsError>>,
    pub looked_up: Mutex<Vec<String>>,
}

impl FakeInputs {
    fn new() -> Self {
        Self {
            facts: Mutex::new(Ok(ActionFacts {
                image: IMAGE.to_owned(),
                timeout: Some(Duration::from_secs(60)),
                system: System::Linux,
                size_class: built_class(),
            })),
            tree: Mutex::new(Ok(TREE.to_owned())),
            looked_up: Mutex::new(Vec::new()),
        }
    }
}

#[tonic::async_trait]
impl Inputs for FakeInputs {
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        locked(&self.looked_up).push(action.hash.clone());
        locked(&self.facts).clone()
    }

    async fn tree(&self, _run: RunId) -> Result<String, InputsError> {
        locked(&self.tree).clone()
    }
}

/// The action tree the fake inputs give a put-directory run.
pub const TREE: &str = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

/// What the frontend makes of a pack: scripted, then the default.
pub struct FakeIngest {
    /// While set, a read waits for `release` (a slow store).
    pub hold: AtomicBool,
    pub release: tokio::sync::Notify,
    pub script: Mutex<VecDeque<Ingested>>,
    /// The reason the next mint fails for, if any.
    pub mint_fails: Mutex<VecDeque<MintError>>,
    /// Each read: the run, the key and the limits.
    pub calls: Mutex<Vec<(RunId, String, UntrustedLimits)>>,
    /// How many keys were minted: each key is new.
    pub minted: AtomicU64,
    /// The form of input tree each mint was asked to write, in order.
    pub forms: Mutex<Vec<InputTree>>,
    /// The wall each mint was asked to keep its directory good for, in order.
    pub walls: Mutex<Vec<Duration>>,
}

impl FakeIngest {
    fn new() -> Self {
        Self {
            hold: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            script: Mutex::new(VecDeque::new()),
            mint_fails: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            minted: AtomicU64::new(0),
            forms: Mutex::new(Vec::new()),
            walls: Mutex::new(Vec::new()),
        }
    }

    pub fn ran(exit_code: i32) -> Ingested {
        Ingested::Ran {
            result: Box::new(ActionResult {
                exit_code,
                ..ActionResult::default()
            }),
            cached: true,
        }
    }

    /// The keys read, in order.
    pub fn keys_read(&self) -> Vec<String> {
        locked(&self.calls).iter().map(|c| c.1.clone()).collect()
    }
}

#[tonic::async_trait]
impl Ingest for FakeIngest {
    async fn mint(
        &self,
        run: RunId,
        _action: &Digest,
        wall: Duration,
        tree: InputTree,
    ) -> Result<PackKey, MintError> {
        locked(&self.forms).push(tree);
        locked(&self.walls).push(wall);
        match locked(&self.mint_fails).pop_front() {
            Some(why) => Err(why),
            None => Ok(PackKey::new(format!(
                "cas/b/{}/{:032x}-0",
                build_id(run),
                self.minted.fetch_add(1, Ordering::SeqCst)
            ))),
        }
    }

    async fn ingest(
        &self,
        run: RunId,
        key: &PackKey,
        _action: &Digest,
        limits: &UntrustedLimits,
    ) -> Ingested {
        locked(&self.calls).push((run, key.to_string(), *limits));
        if self.hold.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        locked(&self.script)
            .pop_front()
            .unwrap_or_else(|| Self::ran(0))
    }
}

/// The state store the executors of one group share: the in-memory store.
pub struct Shared {
    pub store: Arc<Fake>,
}

impl Shared {
    pub fn new() -> Self {
        let clock = Arc::new(ManualClock::default());
        let store = Arc::new(Fake::with_flaw(clock, Flaw::None));
        Self { store }
    }

    /// The store as an executor takes it.
    pub fn handle(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store) as Arc<dyn Store>
    }

    /// The rows of the process `instance`.
    pub fn rows(&self, instance: &str) -> Rows {
        Rows::new(self.handle(), instance)
    }

    /// Every key under `prefix`.
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        let rows = self.store.scan_prefix(prefix).expect("a scan");
        rows.into_iter().map(|(key, _)| key).collect()
    }
}

/// A job service with hosts, and an executor that submits to it.
pub struct Rig {
    pub fake: Arc<FakeJobServer>,
    pub api: Client,
    pub exec: Executor,
    pub cfg: Config,
    pub inputs: Arc<FakeInputs>,
    pub ingest: Arc<FakeIngest>,
    pub metrics: Arc<Metrics>,
    /// The invocation recorder the executor tells.
    pub recorder: Recorder,
}

/// An Execute the rig made: its operation, and where it is.
pub struct Caller {
    pub tag: u8,
    pub name: String,
    pub stage: watch::Receiver<Stage>,
}

impl Caller {
    pub fn finished(&self) -> Option<Finished> {
        match &*self.stage.borrow() {
            Stage::Done(finished) => Some(finished.clone()),
            _ => None,
        }
    }

    /// Waits until the operation's job has started.
    ///
    /// # Panics
    /// It has not within [`PROMPT`].
    pub async fn executing(&mut self) {
        let waited = timeout(
            PROMPT,
            self.stage
                .wait_for(|stage| matches!(stage, Stage::Executing | Stage::Done(_))),
        )
        .await;
        waited
            .unwrap_or_else(|_| panic!("action {} is not executing", self.tag))
            .expect("the operation is open");
    }

    /// Waits until the operation is answered.
    ///
    /// # Panics
    /// It is not answered within [`PROMPT`].
    pub async fn done(&mut self) -> Finished {
        let waited = timeout(
            PROMPT,
            self.stage.wait_for(|stage| matches!(stage, Stage::Done(_))),
        )
        .await;
        let stage = waited
            .unwrap_or_else(|_| panic!("action {} is not answered", self.tag))
            .expect("the operation is open");
        match &*stage {
            Stage::Done(finished) => finished.clone(),
            other => panic!("{other:?}"),
        }
    }
}

pub fn digest_of(tag: u8) -> kbf_types::Digest {
    kbf_types::Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

/// A Linux host of 16 cores, `gib` GiB of memory and 1 TiB of disk that runs containers on
/// `arch`.
pub fn machine(gib: u64, arch: &str) -> Machine {
    Machine {
        shape: Shape {
            cpu_millis: 16_000,
            memory_bytes: gib << 30,
            gpus: 0,
            gpu_class: String::new(),
            disk_bytes: Some(1 << 40),
        },
        os: "linux".to_owned(),
        arch: arch.to_owned(),
        exec: ExecMode::Container,
    }
}

impl Rig {
    /// A rig over a job service of `hosts` (each a [`machine`] of 64 GiB on `x86_64`), and an
    /// executor with `adjust` applied to its config.
    pub async fn start(hosts: &[&str], adjust: impl FnOnce(&mut Config)) -> Self {
        Self::over(hosts, None, adjust).await
    }

    /// As [`Rig::start`], keeping the executor's rows in `store`.
    pub async fn over(
        hosts: &[&str],
        store: Option<Arc<dyn Store>>,
        adjust: impl FnOnce(&mut Config),
    ) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let fake = FakeJobServer::start().await.expect("a loopback port");
        for host in hosts {
            fake.add_host(host, machine(64, "x86_64"));
        }
        let api = fake.client().expect("a client");
        let mut cfg = Config::new("front-1");
        cfg.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
        cfg.poll = POLL;
        adjust(&mut cfg);
        let (recorder, recording) = Recorder::new(1 << 20, 256);
        tokio::spawn(recording.run());
        let fake = Arc::new(fake);
        let (inputs, ingest) = (Arc::new(FakeInputs::new()), Arc::new(FakeIngest::new()));
        let metrics = Arc::new(Metrics::default());
        let deps = Deps {
            client: fake.client().expect("a client"),
            ingest: ingest.clone(),
            inputs: inputs.clone(),
            metrics: Arc::clone(&metrics),
            store,
            recorder: recorder.clone(),
        };
        let exec = Executor::new(cfg.clone(), deps).expect("a valid config");
        Self {
            fake,
            api,
            exec,
            cfg,
            inputs,
            ingest,
            metrics,
            recorder,
        }
    }

    /// An executor of `cfg` over this rig's job service and fakes.
    pub fn executor_of(&self, cfg: Config, store: Option<Arc<dyn Store>>) -> Executor {
        let deps = Deps {
            client: self.fake.client().expect("a client"),
            ingest: self.ingest.clone(),
            inputs: self.inputs.clone(),
            metrics: Arc::clone(&self.metrics),
            store,
            recorder: self.recorder.clone(),
        };
        Executor::new(cfg, deps).expect("a valid config")
    }

    /// A new executor over the same job service, of the frontend `frontend` and a process of
    /// its own.
    pub fn other_executor(&self, frontend: &str, store: Option<Arc<dyn Store>>) -> Executor {
        let mut cfg = self.cfg.clone();
        let fresh = Config::new(frontend);
        cfg.frontend = fresh.frontend;
        cfg.instance = fresh.instance;
        self.executor_of(cfg, store)
    }

    /// Submits the action of `tag` to `exec`; `whole` asks for the whole machine, and `call`
    /// is the call the invocation recorder is told of.
    pub fn submit_to(
        &self,
        exec: &Executor,
        tag: u8,
        whole: bool,
        call: Option<ExecuteCall>,
    ) -> Caller {
        let key = ActionKey {
            instance: "main".to_owned(),
            action: digest_of(tag),
        };
        let ticket = exec
            .submit(Submission {
                key,
                whole_machine: whole,
                call,
                described: Described {
                    arguments: vec!["/bin/run".to_owned()],
                    outputs: Vec::new(),
                    label: Default::default(),
                },
                evidence: Default::default(),
            })
            .expect("the executor takes the action");
        Caller {
            tag,
            name: ticket.name,
            stage: ticket.stage,
        }
    }

    /// Submits the action of `tag`.
    pub fn submit(&self, tag: u8) -> Caller {
        self.submit_to(&self.exec, tag, false, None)
    }

    /// Submits the action of `tag`, which asks for the whole machine.
    pub fn submit_whole(&self, tag: u8) -> Caller {
        self.submit_to(&self.exec, tag, true, None)
    }

    /// Submits the action of `tag` as a call of the invocation `invocation`, which the recorder
    /// is told of.
    pub fn submit_called(&self, tag: u8, invocation: &str) -> Caller {
        let metadata = CallMetadata {
            invocation_id: InvocationId::parse(invocation),
            ..CallMetadata::default()
        };
        let call = ExecuteCall::new(
            &self.recorder,
            &metadata,
            &AuthenticationMetadata::default(),
            "main",
            digest_of(tag),
            digest_of(tag.wrapping_add(100)),
        );
        assert!(call.is_some(), "a call with an invocation id is recorded");
        self.submit_to(&self.exec, tag, false, call)
    }

    /// Every job the job service holds, with what it was submitted as, in the order they were
    /// recorded.
    pub fn jobs(&self) -> Vec<Posted> {
        self.fake
            .jobs()
            .into_iter()
            .map(|job| {
                let spec = self.fake.submitted(&job.id).unwrap_or_default();
                Posted { job, spec }
            })
            .collect()
    }

    /// The jobs of the action of `tag`, in the order they were submitted.
    pub fn jobs_of(&self, tag: u8) -> Vec<Posted> {
        let want = format!("{}/{}", hex(tag), tag);
        self.jobs()
            .into_iter()
            .filter(|p| p.arg("--action") == Some(want.as_str()))
            .collect()
    }

    /// Waits until the job service holds `n` jobs of the action of `tag`.
    ///
    /// # Panics
    /// They are not there within [`PROMPT`].
    pub async fn until_jobs(&self, tag: u8, n: usize) -> Vec<Posted> {
        let found = async {
            loop {
                let jobs = self.jobs_of(tag);
                if jobs.len() >= n {
                    return jobs;
                }
                sleep(POLL).await;
            }
        };
        match timeout(PROMPT, found).await {
            Ok(jobs) => jobs,
            Err(_) => panic!(
                "{n} jobs of action {tag} not made: {:?}\ncalls {:?}",
                self.jobs_of(tag),
                self.methods()
            ),
        }
    }

    /// The first job of the action of `tag`, once the executor has submitted it.
    pub async fn first_job(&self, tag: u8) -> Posted {
        self.until_jobs(tag, 1).await.remove(0)
    }

    /// The host `host` takes the queued job `id` and runs it.
    pub fn host_runs(&self, id: &str, host: &str) {
        assert!(self.fake.place(id, host), "{id} is placed");
        assert!(self.fake.starting(id), "{id} is starting");
        assert!(self.fake.run(id), "{id} is running");
    }

    /// The queued job `id` runs on `host` to its end with `exit_code`.
    pub fn host_finishes(&self, id: &str, host: &str, exit_code: i32) {
        self.host_runs(id, host);
        assert!(self.fake.finish(id, exit_code), "{id} finishes");
    }

    /// The running job `id` ends with exit code 0.
    pub fn host_finishes_running(&self, id: &str) {
        assert!(self.fake.finish(id, 0), "{id} finishes");
    }

    /// Every call the job service read, by method name, in order.
    pub fn methods(&self) -> Vec<&'static str> {
        self.fake.calls().iter().map(method).collect()
    }

    /// What the executor counts, as `GET /metrics` shows it.
    pub fn counted(&self) -> String {
        let mut text = String::new();
        self.metrics.render(&mut text);
        text
    }

    /// The value of the counter `name` in [`Rig::counted`].
    pub fn counter(&self, name: &str) -> u64 {
        let text = self.counted();
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or_else(|| panic!("no counter {name} in\n{text}"))
    }

    /// Waits until the counter `name` is at least `at_least`.
    ///
    /// # Panics
    /// It is not within [`PROMPT`].
    pub async fn counter_reaches(&self, name: &str, at_least: u64) {
        let reached = async {
            while self.counter(name) < at_least {
                sleep(POLL).await;
            }
        };
        if timeout(PROMPT, reached).await.is_err() {
            panic!("{name} is below {at_least} in\n{}", self.counted());
        }
    }

    /// Every action the rig is asked about runs for `timeout`.
    pub fn set_timeout(&self, timeout: Duration) {
        locked(&self.inputs.facts)
            .as_mut()
            .expect("ok facts")
            .timeout = Some(timeout);
    }

    /// The next read of the job `id` and of its result are answered as a job that ended in
    /// `job_state` and a result of `result_state`, for `reason`, with `exit_code`, whatever the
    /// job service holds.
    pub fn script_ending(
        &self,
        id: &str,
        (job_state, result_state, reason, exit_code): (
            JobState,
            JobState,
            ResultReason,
            Option<i32>,
        ),
    ) {
        let job = GetJobResponse {
            job: Some(Job {
                id: id.to_owned(),
                state: job_state.into(),
                ..Job::default()
            }),
        };
        let result = GetResultResponse {
            result: Some(JobResult {
                state: result_state.into(),
                exit_code,
                reason: reason.into(),
                ..JobResult::default()
            }),
        };
        self.fake
            .inject_on("GetJob", Fault::Answer(Bytes::from(job.encode_to_vec())));
        self.fake.inject_on(
            "GetResult",
            Fault::Answer(Bytes::from(result.encode_to_vec())),
        );
    }

    /// As [`Rig::script_ending`] for a job that failed for `reason` with `exit_code`.
    pub fn script_failed(&self, id: &str, reason: ResultReason, exit_code: i32) {
        self.script_ending(
            id,
            (JobState::Failed, JobState::Failed, reason, Some(exit_code)),
        );
    }
}

/// The method of `call`.
pub fn method(call: &Call) -> &'static str {
    match call {
        Call::SubmitJob(_) => "SubmitJob",
        Call::GetJob(_) => "GetJob",
        Call::ListJobs(_) => "ListJobs",
        Call::CancelJob(_) => "CancelJob",
        Call::GetResult(_) => "GetResult",
        Call::GetLogs(_) => "GetLogs",
    }
}

/// The job ids of the `CancelJob`s among `calls`, in order.
pub fn cancels(calls: &[Call]) -> Vec<String> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::CancelJob(request) => Some(request.job_id.clone()),
            _ => None,
        })
        .collect()
}

/// The specs of the `SubmitJob`s among `calls`, in order.
pub fn submits(calls: &[Call]) -> Vec<JobSpec> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::SubmitJob(request) => request.spec.clone(),
            _ => None,
        })
        .collect()
}

pub fn hex(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

/// A job as the job service holds it, and the spec it was submitted with.
#[derive(Clone, Debug)]
pub struct Posted {
    pub job: Job,
    pub spec: JobSpec,
}

impl Posted {
    /// The job's id.
    pub fn id(&self) -> &str {
        &self.job.id
    }

    /// The value after `flag` in the task tool's arguments.
    pub fn arg(&self, flag: &str) -> Option<&str> {
        let args = &self.spec.args;
        let at = args.iter().position(|a| a == flag)?;
        args.get(at + 1).map(String::as_str)
    }
}
