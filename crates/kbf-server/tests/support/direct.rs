//! The executor side of the harness: a fake job service, fakes of the two seams the executor
//! reads the rest of the server through, and the helpers that drive a job. `support/mod.rs`
//! documents how a test uses them.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_proto::v1::{
    ExecMode, GetJobResponse, GetResultResponse, Job, JobResult, JobState, ResultReason, Shape,
};
use kbf_front::{Cache, Recorder, VerifiedBlob};
use kbf_job_client::fake::{FakeJobServer, Fault, Machine};
use kbf_proto::reapi::{ActionResult, Digest, OutputFile};
use kbf_server::executor::{
    ActionFacts, Config, Deps, Executor, Ingest, Ingested, InputTree, Inputs, InputsError, Metrics,
    MintError, PackKey, PutDirRoots, RunId, SizeClass, System, UntrustedLimits, build_id,
};
use prost::Message;
use tokio::time::{sleep, timeout};

use super::{Blob, GateLog, PROMPT, SharedStore};

/// The one host of the fake job service.
pub const HOST: &str = "host-1";

/// The image every action of the fake inputs runs in.
pub const IMAGE: &str =
    "registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the task tool's pack says an action did: the result the fake ingest gives back (and,
/// when the exit code is 0, writes to the action cache with its outputs in the CAS).
#[derive(Clone, Debug, Default)]
pub struct Ran {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    /// Each output file: its path and its bytes.
    pub outputs: Vec<(String, Vec<u8>)>,
}

impl Ran {
    /// An action that exited `exit_code` and wrote nothing.
    pub fn exits(exit_code: i32) -> Self {
        Self {
            exit_code,
            ..Self::default()
        }
    }

    /// An action that exited 0 and wrote the output `path` with `bytes`.
    pub fn writes(path: &str, bytes: &str) -> Self {
        Self::exits(0).output(path, bytes)
    }

    /// Also writes the output `path` with `bytes`.
    #[must_use]
    pub fn output(mut self, path: &str, bytes: &str) -> Self {
        self.outputs
            .push((path.to_owned(), bytes.as_bytes().to_vec()));
        self
    }

    /// Also wrote `bytes` to its standard output.
    #[must_use]
    pub fn stdout(mut self, bytes: &str) -> Self {
        self.stdout = bytes.as_bytes().to_vec();
        self
    }

    /// The result this is: its blobs are not stored here (see [`Direct::ingest`]).
    fn result(&self, digest: &impl Fn(&[u8]) -> Digest) -> ActionResult {
        ActionResult {
            exit_code: self.exit_code,
            output_files: self
                .outputs
                .iter()
                .map(|(path, bytes)| OutputFile {
                    path: path.clone(),
                    digest: Some(digest(bytes)),
                    ..Default::default()
                })
                .collect(),
            stdout_digest: (!self.stdout.is_empty()).then(|| digest(&self.stdout)),
            ..Default::default()
        }
    }
}

/// What the fake ingest was asked, in order.
#[derive(Clone, Debug)]
pub struct Ingestion {
    /// The run whose pack was read: `<operation>-<attempt>`.
    pub run: String,
    /// The hash of the action.
    pub action: String,
}

/// What the tests script, and what the fakes saw.
#[derive(Default)]
struct Script {
    /// Each action's next results, by action hash.
    next: BTreeMap<String, VecDeque<Ran>>,
    /// What an action with nothing scripted does.
    otherwise: Option<Ran>,
    ingestions: Vec<Ingestion>,
    mint_fails: VecDeque<MintError>,
    facts_timeout: Option<Duration>,
}

/// The job service and the scripts an executor of a test server runs over. It outlives a
/// restart of the server: a new executor is built over the same job service.
#[derive(Clone)]
pub struct Direct {
    /// The fake job service, with one Linux host ([`HOST`]). A test may inject faults, advance
    /// its clock and read its calls.
    pub fake: Arc<FakeJobServer>,
    /// What the executor counts.
    pub metrics: Arc<Metrics>,
    script: Arc<Mutex<Script>>,
    minted: Arc<AtomicU64>,
}

/// A Linux host of 16 cores and 64 GiB that runs containers on `x86_64`.
pub fn machine() -> Machine {
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

impl Direct {
    /// A job service on loopback with the host [`HOST`], and nothing scripted: every action
    /// exits 0 and writes nothing.
    pub async fn start() -> Self {
        let fake = FakeJobServer::start().await.expect("a loopback port");
        fake.add_host(HOST, machine());
        Self {
            fake: Arc::new(fake),
            metrics: Arc::new(Metrics::default()),
            script: Arc::new(Mutex::new(Script::default())),
            minted: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The executor over this job service for a server over `cache`, keeping its rows in `store`
    /// if given, alive for `live` (the default when none). Built on the server's own runtime, where it is used.
    pub fn executor(
        &self,
        cache: Arc<Cache<GateLog, SharedStore>>,
        store: Option<Arc<dyn kbf_state::Store>>,
        finished_retention: Duration,
        live: Option<Duration>,
    ) -> Executor {
        let mut cfg = Config::new("front-1");
        cfg.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
        cfg.poll = Duration::from_millis(20);
        cfg.finished_retention = finished_retention;
        if let Some(live) = live {
            cfg.live = live;
        }
        let seams = Arc::new(Seams {
            direct: self.clone(),
            cache,
        });
        let deps = Deps {
            client: self.fake.client().expect("a job client"),
            ingest: seams.clone(),
            inputs: seams,
            metrics: Arc::clone(&self.metrics),
            store,
            recorder: Recorder::off(),
        };
        Executor::new(cfg, deps).expect("a valid executor config")
    }

    // ---- what the actions do

    /// The action `action`'s next run did `ran`. Scripts for one action are used in order.
    pub fn will_run(&self, action: &Blob, ran: Ran) {
        locked(&self.script)
            .next
            .entry(action.proto.hash.clone())
            .or_default()
            .push_back(ran);
    }

    /// Every action with nothing scripted does `ran`.
    pub fn will_run_any(&self, ran: Ran) {
        locked(&self.script).otherwise = Some(ran);
    }

    /// The next mint of a pack key fails with `error`.
    pub fn fail_next_mint(&self, error: MintError) {
        locked(&self.script).mint_fails.push_back(error);
    }

    /// Every action's facts name `timeout` as the action's own.
    pub fn set_action_timeout(&self, timeout: Duration) {
        locked(&self.script).facts_timeout = Some(timeout);
    }

    /// The packs read so far, in order.
    pub fn ingestions(&self) -> Vec<Ingestion> {
        locked(&self.script).ingestions.clone()
    }

    // ---- the jobs

    /// Every job of the action `action`, as the job service holds them, in the order they were
    /// submitted: its attempts.
    pub fn jobs_of(&self, action: &Blob) -> Vec<Job> {
        let want = format!("{}/{}", action.proto.hash, action.proto.size_bytes);
        self.fake
            .jobs()
            .into_iter()
            .filter(|job| {
                let spec = self.fake.submitted(&job.id).unwrap_or_default();
                let at = spec.args.iter().position(|a| a == "--action");
                at.and_then(|at| spec.args.get(at + 1)) == Some(&want)
            })
            .collect()
    }

    /// How many jobs the job service has been submitted, of any action.
    pub fn submitted_jobs(&self) -> usize {
        self.fake.jobs().len()
    }

    /// The id of the job of `action`'s attempt `attempt` (from 0), once the executor has
    /// submitted it.
    ///
    /// # Panics
    /// The job is not submitted within [`PROMPT`].
    pub async fn job(&self, action: &Blob, attempt: u32) -> String {
        let suffix = format!("-{attempt}");
        let found = async {
            loop {
                let jobs = self.jobs_of(action);
                if let Some(job) = jobs.iter().find(|job| job.id.ends_with(&suffix)) {
                    return job.id.clone();
                }
                sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(PROMPT, found)
            .await
            .unwrap_or_else(|_| panic!("attempt {attempt} of the action is submitted"))
    }

    /// The host takes `job`, starts it and runs it.
    pub fn run_job(&self, job: &str) {
        assert!(self.fake.place(job, HOST), "{job} is placed");
        assert!(self.fake.starting(job), "{job} is starting");
        assert!(self.fake.run(job), "{job} is running");
    }

    /// Waits for the first attempt of `action` and runs it. Returns its id.
    pub async fn run(&self, action: &Blob) -> String {
        let job = self.job(action, 0).await;
        self.run_job(&job);
        job
    }

    /// Runs the first attempt of `action` to the end: the task tool exits `exit_code` (0 is
    /// what the pack is ingested after, whatever the action's own exit code). Returns its id.
    pub async fn finish(&self, action: &Blob, exit_code: i32) -> String {
        let job = self.run(action).await;
        assert!(self.fake.finish(&job, exit_code), "{job} finishes");
        job
    }

    /// Runs the first attempt of `action` to a clean end.
    pub async fn complete(&self, action: &Blob) -> String {
        self.finish(action, 0).await
    }

    /// Runs attempt `attempt` of `action` and its host is lost.
    pub async fn lose(&self, action: &Blob, attempt: u32) -> String {
        let job = self.job(action, attempt).await;
        self.run_job(&job);
        assert!(self.fake.lose(&job), "{job} is lost");
        job
    }

    /// Runs the first attempt of `action` and has it end at the wall limit the job asked for,
    /// which is the action's timeout: the next reads of the job and of its result say so.
    pub async fn time_out(&self, action: &Blob) -> String {
        let job = self.run(action).await;
        let mut ended = self
            .fake
            .jobs()
            .into_iter()
            .find(|j| j.id == job)
            .expect("the job");
        ended.set_state(JobState::Failed);
        let mut result = JobResult::default();
        result.set_state(JobState::Failed);
        result.set_reason(ResultReason::WallLimit);
        let job_answer = GetJobResponse { job: Some(ended) };
        let result_answer = GetResultResponse {
            result: Some(result),
        };
        self.fake
            .inject_on("GetJob", Fault::Answer(job_answer.encode_to_vec().into()));
        self.fake.inject_on(
            "GetResult",
            Fault::Answer(result_answer.encode_to_vec().into()),
        );
        job
    }

    /// The methods the job service was called with, in order.
    pub fn methods(&self) -> Vec<&'static str> {
        use kbf_job_client::fake::Call;
        self.fake
            .calls()
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
}

/// The executor's two seams, over the server's cache.
struct Seams {
    direct: Direct,
    cache: Arc<Cache<GateLog, SharedStore>>,
}

fn proto_digest(bytes: &[u8]) -> Digest {
    Blob::new(bytes).proto
}

#[tonic::async_trait]
impl Inputs for Seams {
    async fn action(&self, _action: &Digest) -> Result<ActionFacts, InputsError> {
        let out = "buck-out/v2/art/root/7a3c1e9f0b2d4c68/pkg/__t__/o".to_owned();
        let shape = kbf_server::action_usage::Shape::of(&["cc".to_owned()], &[out]);
        Ok(ActionFacts {
            image: IMAGE.to_owned(),
            timeout: Some(
                locked(&self.direct.script)
                    .facts_timeout
                    .unwrap_or(Duration::from_secs(60)),
            ),
            system: System::Linux,
            size_class: SizeClass::of_action(&shape),
        })
    }

    async fn tree(&self, _run: RunId) -> Result<String, InputsError> {
        Ok("ab".repeat(32))
    }
}

#[tonic::async_trait]
impl Ingest for Seams {
    async fn mint(
        &self,
        run: RunId,
        _action: &Digest,
        _wall: Duration,
        _tree: InputTree,
    ) -> Result<PackKey, MintError> {
        if let Some(why) = locked(&self.direct.script).mint_fails.pop_front() {
            return Err(why);
        }
        let nonce = self.direct.minted.fetch_add(1, Ordering::SeqCst);
        Ok(PackKey::new(format!(
            "cas/b/{}/{nonce:032x}-0",
            build_id(run)
        )))
    }

    /// What the frontend's ingest does of a pack: the scripted [`Ran`]'s blobs go to the CAS
    /// and, for an action that exited 0, its result to the action cache.
    async fn ingest(
        &self,
        run: RunId,
        _key: &PackKey,
        action: &Digest,
        _limits: &UntrustedLimits,
    ) -> Ingested {
        let ran = {
            let mut script = locked(&self.direct.script);
            script.ingestions.push(Ingestion {
                run: build_id(run),
                action: action.hash.clone(),
            });
            script
                .next
                .get_mut(&action.hash)
                .and_then(VecDeque::pop_front)
                .or_else(|| script.otherwise.clone())
                .unwrap_or_default()
        };
        let blobs = ran
            .outputs
            .iter()
            .map(|(_, bytes)| bytes.clone())
            .chain((!ran.stdout.is_empty()).then(|| ran.stdout.clone()))
            .map(|bytes| VerifiedBlob::hashed(bytes.into()))
            .collect::<Vec<_>>();
        if let Err(why) = self.cache.store_blobs(blobs).await {
            return Ingested::Unavailable(why.to_string());
        }
        let result = ran.result(&proto_digest);
        let cached = ran.exit_code == 0;
        if cached {
            let key = kbf_front::digest_from_proto(Some(action)).expect("an action digest");
            let written = self
                .cache
                .write_action_result(kbf_meta::Role::Daemon, key, &result)
                .await;
            if let Err(why) = written {
                return Ingested::Unavailable(why.to_string());
            }
        }
        Ingested::Ran {
            result: Box::new(result),
            cached,
        }
    }
}
