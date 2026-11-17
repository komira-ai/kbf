//! The dispatcher and the rig the other tests of the executor's tasks run on: a real job service
//! (the fake of `kbf-job-client`, on a loopback port picked by the system), fakes of the three
//! seams, and tokio's paused clock, which only the tests move.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_proto::v1::{ExecMode, Shape};
use kbf_front::{ActionLabel, Described, Finished, Recorder, Stage, Submission, Ticket};
use kbf_job_client::fake::{Call, FakeJobServer, Machine};
use kbf_proto::reapi::ActionResult;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, OperationId};
use tokio::sync::watch;

use super::*;
use crate::executor::task::{RunId, build_id};

pub(crate) const IMAGE: &str =
    "registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the executor reads of an action, and the tree of its run.
pub(crate) struct FakeInputs {
    pub facts: Mutex<Result<ActionFacts, InputsError>>,
    pub tree: Mutex<Result<String, InputsError>>,
    /// While set, the read of an action never answers (a CAS that hangs).
    pub hang: AtomicBool,
    /// While set, the read of an action panics (a seam that breaks).
    pub panics: AtomicBool,
    /// How many actions were read.
    pub looked_up: AtomicU64,
}

impl FakeInputs {
    fn new() -> Self {
        let shape = crate::action_usage::Shape::of(
            &["cc".to_owned()],
            &["buck-out/v2/art/root/7a3c1e9f0b2d4c68/pkg/__t__/o".to_owned()],
        );
        Self {
            facts: Mutex::new(Ok(ActionFacts {
                image: IMAGE.to_owned(),
                timeout: Some(Duration::from_secs(60)),
                system: System::Linux,
                size_class: SizeClass::of_action(&shape),
            })),
            tree: Mutex::new(Ok("ef".repeat(32))),
            hang: AtomicBool::new(false),
            panics: AtomicBool::new(false),
            looked_up: AtomicU64::new(0),
        }
    }
}

#[tonic::async_trait]
impl Inputs for FakeInputs {
    async fn action(&self, _action: &kbf_proto::reapi::Digest) -> Result<ActionFacts, InputsError> {
        self.looked_up.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        assert!(
            !self.panics.load(Ordering::SeqCst),
            "the read of the action broke"
        );
        locked(&self.facts).clone()
    }

    async fn tree(&self, _run: RunId) -> Result<String, InputsError> {
        locked(&self.tree).clone()
    }
}

/// What the frontend makes of a pack: scripted, then a clean run.
pub(crate) struct FakeIngest {
    pub script: Mutex<VecDeque<Ingested>>,
    pub mint_fails: Mutex<VecDeque<MintError>>,
    pub minted: AtomicU64,
    /// Every pack read: the run and the key.
    pub ingested: Mutex<Vec<(RunId, String)>>,
}

impl FakeIngest {
    pub(crate) fn ran(exit_code: i32) -> Ingested {
        Ingested::Ran {
            result: Box::new(ActionResult {
                exit_code,
                ..ActionResult::default()
            }),
            cached: true,
        }
    }
}

#[tonic::async_trait]
impl Ingest for FakeIngest {
    async fn mint(
        &self,
        run: RunId,
        _action: &kbf_proto::reapi::Digest,
        _wall: Duration,
        _tree: InputTree,
    ) -> Result<PackKey, MintError> {
        if let Some(why) = locked(&self.mint_fails).pop_front() {
            return Err(why);
        }
        let n = self.minted.fetch_add(1, Ordering::SeqCst);
        Ok(PackKey::new(format!("cas/b/{}/{n:032x}", build_id(run))))
    }

    async fn ingest(
        &self,
        run: RunId,
        key: &PackKey,
        _action: &kbf_proto::reapi::Digest,
        _limits: &UntrustedLimits,
    ) -> Ingested {
        locked(&self.ingested).push((run, key.to_string()));
        locked(&self.script)
            .pop_front()
            .unwrap_or_else(|| Self::ran(0))
    }
}

/// A state store whose calls can be made to fail, to panic, or to lose a race.
pub(crate) struct Faulty {
    pub inner: Fake,
    pub reads_fail: AtomicBool,
    pub writes_fail: AtomicBool,
    pub writes_panic: AtomicBool,
    pub reads_panic: AtomicBool,
    /// The read that fails alone, counting from 1: 0 for none.
    pub fail_read: AtomicU64,
    /// The write that fails alone, counting from 1: 0 for none.
    pub fail_write: AtomicU64,
    /// A key written (its own value again) by another process just before each write, so the
    /// write's precondition on that key fails.
    pub race: Mutex<Option<String>>,
    /// Run once, just before the first read of a key under the prefix: another process's
    /// write landing between two reads of a caller.
    pub before_read: Mutex<Option<(String, BeforeRead)>>,
}

/// What [`Faulty::before_read`] runs.
pub(crate) type BeforeRead = Box<dyn FnOnce() + Send>;

impl Faulty {
    pub(crate) fn new(clock: Arc<ManualClock>) -> Self {
        Self {
            inner: Fake::with_flaw(clock, Flaw::None),
            reads_fail: AtomicBool::new(false),
            writes_fail: AtomicBool::new(false),
            writes_panic: AtomicBool::new(false),
            reads_panic: AtomicBool::new(false),
            fail_read: AtomicU64::new(0),
            fail_write: AtomicU64::new(0),
            race: Mutex::new(None),
            before_read: Mutex::new(None),
        }
    }

    fn read_fault(&self) -> Result<(), StoreError> {
        assert!(!self.reads_panic.load(Ordering::SeqCst), "the store broke");
        let left = self.fail_read.load(Ordering::SeqCst);
        if left == 1 {
            self.fail_read.store(0, Ordering::SeqCst);
            return Err(StoreError::Failed("this read failed".to_owned()));
        }
        if left > 1 {
            self.fail_read.store(left - 1, Ordering::SeqCst);
        }
        match self.reads_fail.load(Ordering::SeqCst) {
            true => Err(StoreError::Failed("the read failed".to_owned())),
            false => Ok(()),
        }
    }

    /// Fails the `nth` read from now on, counting from 1, and no other.
    pub(crate) fn fail_nth_read(&self, nth: u64) {
        self.fail_read.store(nth, Ordering::SeqCst);
    }

    /// Fails the `nth` write from now on, counting from 1, and no other.
    pub(crate) fn fail_nth_write(&self, nth: u64) {
        self.fail_write.store(nth, Ordering::SeqCst);
    }
}

impl Store for Faulty {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        assert!(!self.writes_panic.load(Ordering::SeqCst), "the store broke");
        if self.writes_fail.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the write failed".to_owned()));
        }
        let left = self.fail_write.load(Ordering::SeqCst);
        if left == 1 {
            self.fail_write.store(0, Ordering::SeqCst);
            return Err(StoreError::Failed("this write failed".to_owned()));
        }
        if left > 1 {
            self.fail_write.store(left - 1, Ordering::SeqCst);
        }
        let raced = locked(&self.race).clone();
        if let Some(key) = raced {
            let value = self
                .inner
                .get(&key)?
                .map_or_else(|| vec![0; 8], |found| found.value);
            let _ = self
                .inner
                .txn(Txn::new().put(key, value, kbf_state::Cond::Any))?;
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.read_fault()?;
        let armed = {
            let mut hook = locked(&self.before_read);
            match hook.as_ref() {
                Some((prefix, _)) if key.starts_with(prefix.as_str()) => hook.take(),
                _ => None,
            }
        };
        if let Some((_, run)) = armed {
            run();
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.read_fault()?;
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// Holds tokio's paused clock where it is. The paused clock jumps to its next timer whenever
/// the runtime waits, which a call to the job service on loopback does; a blocking task in
/// flight stops the jump. The tests move the clock by hand, and so assert on what happened,
/// never on how long it took.
pub(crate) struct ClockHold {
    release: Option<std::sync::mpsc::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ClockHold {
    fn new() -> Self {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let task = tokio::task::spawn_blocking(move || {
            let _ = hold.recv();
        });
        Self {
            release: Some(release),
            task: Some(task),
        }
    }
}

impl Drop for ClockHold {
    fn drop(&mut self) {
        self.release.take().iter().for_each(|release| {
            let _ = release.send(());
        });
        self.task.take();
    }
}

/// Lets the tasks the executor spawned run to rest, and the calls they have in flight to the
/// job service on loopback be answered. Each round waits on blocking tasks that do nothing,
/// which lets the runtime park on its I/O driver as one with nothing to run does. No time is
/// waited for, real or virtual.
pub(crate) async fn settle() {
    for _ in 0..48 {
        let _ = tokio::task::spawn_blocking(|| ()).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }
}

/// What a rig is built with.
#[derive(Default)]
pub(crate) struct Setup {
    /// A state store the executor keeps its rows in.
    pub store: bool,
    /// An invocation recorder that is on.
    pub record: bool,
}

/// A caller's handle on an operation.
pub(crate) struct Caller {
    pub name: String,
    pub stage: watch::Receiver<Stage>,
}

impl Caller {
    pub(crate) fn of(ticket: Ticket) -> Self {
        Self {
            name: ticket.name,
            stage: ticket.stage,
        }
    }

    pub(crate) fn finished(&self) -> Option<Finished> {
        match &*self.stage.borrow() {
            Stage::Done(finished) => Some(finished.clone()),
            _ => None,
        }
    }

    pub(crate) fn seen(&self) -> Stage {
        self.stage.borrow().clone()
    }

    pub(crate) fn operation(&self) -> OperationId {
        parse_stored_name(&self.name).expect("a name of an operation")
    }
}

/// The action of `tag`.
pub(crate) fn digest_of(tag: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

/// The Execute of `action`.
pub(crate) fn submission_of(action: Digest) -> Submission {
    Submission {
        key: ActionKey {
            instance: "main".to_owned(),
            action,
        },
        whole_machine: false,
        call: None,
        described: Described {
            arguments: vec!["/bin/run".to_owned()],
            outputs: Vec::new(),
            label: ActionLabel::default(),
        },
        evidence: kbf_invocation::Evidence::default(),
    }
}

/// An executor over a job service, with fakes for what it reads.
pub(crate) struct Rig {
    pub fake: FakeJobServer,
    pub exec: Executor,
    pub cfg: Config,
    pub inputs: Arc<FakeInputs>,
    pub ingest: Arc<FakeIngest>,
    pub metrics: Arc<Metrics>,
    pub recorder: Recorder,
    pub store: Option<Arc<Faulty>>,
    pub clock: Arc<ManualClock>,
    _hold: ClockHold,
}

impl Rig {
    pub(crate) async fn start() -> Self {
        Self::build(Setup::default(), |_| {}).await
    }

    pub(crate) async fn with(adjust: impl FnOnce(&mut Config)) -> Self {
        Self::build(Setup::default(), adjust).await
    }

    pub(crate) async fn stored() -> Self {
        Self::build(
            Setup {
                store: true,
                record: false,
            },
            |_| {},
        )
        .await
    }

    pub(crate) async fn build(setup: Setup, adjust: impl FnOnce(&mut Config)) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let hold = ClockHold::new();
        let fake = FakeJobServer::start().await.expect("a loopback port");
        fake.add_host(
            "h1",
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
            },
        );
        let mut cfg = Config::new("front-1");
        cfg.instance = "p-main".to_owned();
        cfg.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
        adjust(&mut cfg);
        let clock = Arc::new(ManualClock::default());
        let store = setup
            .store
            .then(|| Arc::new(Faulty::new(Arc::clone(&clock))));
        let recorder = if setup.record {
            let (recorder, recording) = Recorder::new(1 << 20, 4096);
            tokio::spawn(recording.run());
            recorder
        } else {
            Recorder::off()
        };
        let inputs = Arc::new(FakeInputs::new());
        let ingest = Arc::new(FakeIngest {
            script: Mutex::new(VecDeque::new()),
            mint_fails: Mutex::new(VecDeque::new()),
            minted: AtomicU64::new(0),
            ingested: Mutex::new(Vec::new()),
        });
        let metrics = Arc::new(Metrics::default());
        let mut rig = Self {
            exec: Executor::new(
                cfg.clone(),
                Deps {
                    client: fake.client().expect("a client"),
                    ingest: ingest.clone(),
                    inputs: inputs.clone(),
                    metrics: Arc::clone(&metrics),
                    store: store.clone().map(|s| s as Arc<dyn Store>),
                    recorder: recorder.clone(),
                },
            )
            .expect("a valid config"),
            fake,
            cfg,
            inputs,
            ingest,
            metrics,
            recorder,
            store,
            clock,
            _hold: hold,
        };
        rig.exec = rig.executor(&rig.cfg.instance.clone());
        rig
    }

    /// The dependencies of an executor of this rig.
    pub(crate) fn deps(&self) -> Deps {
        Deps {
            client: self.fake.client().expect("a client"),
            ingest: self.ingest.clone(),
            inputs: self.inputs.clone(),
            metrics: Arc::clone(&self.metrics),
            store: self.store.clone().map(|s| s as Arc<dyn Store>),
            recorder: self.recorder.clone(),
        }
    }

    /// An executor of this rig's frontend and job service and state store, as the process
    /// `instance`.
    pub(crate) fn executor(&self, instance: &str) -> Executor {
        let mut cfg = self.cfg.clone();
        cfg.instance = instance.to_owned();
        Executor::new(cfg, self.deps()).expect("a valid config")
    }

    /// The rows of the process `instance` over this rig's store.
    pub(crate) fn rows(&self, instance: &str) -> Rows {
        let store = self.store.clone().expect("a rig with a state store");
        Rows::new(store as Arc<dyn Store>, instance)
    }

    /// Time passes: tokio's clock, the job service's and the state store's, and the tasks run
    /// to rest after it.
    pub(crate) async fn advance(&self, by: Duration) {
        tokio::time::advance(by).await;
        self.fake.advance(by);
        self.clock.advance(by);
        settle().await;
    }

    /// Lets the tasks run, and one poll pass, until `ready`.
    pub(crate) async fn until(&self, what: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..400 {
            settle().await;
            if ready() {
                return;
            }
            self.advance(self.cfg.poll).await;
        }
        panic!("never: {what}");
    }

    /// Runs until `caller` is answered, and says how.
    pub(crate) async fn until_done(&self, caller: &Caller) -> Finished {
        self.until("the operation is answered", || caller.finished().is_some())
            .await;
        caller.finished().expect("answered")
    }

    /// Executes the action of `tag` on this rig's executor.
    pub(crate) fn submit(&self, tag: u8) -> Caller {
        Caller::of(
            self.exec
                .submit(submission_of(digest_of(tag)))
                .expect("the executor takes the action"),
        )
    }

    /// The id of the job of attempt `attempt` of `caller`'s operation.
    pub(crate) fn job_of(&self, caller: &Caller, attempt: u32) -> String {
        job_id(&self.cfg.frontend, RunId::new(caller.operation(), attempt))
    }

    /// Waits until the job of that attempt is at the job service.
    pub(crate) async fn submitted(&self, caller: &Caller, attempt: u32) -> String {
        let job = self.job_of(caller, attempt);
        self.until("the job is submitted", || {
            self.fake.submitted(&job).is_some()
        })
        .await;
        job
    }

    /// The host takes the queued `job` and runs it.
    pub(crate) fn host_runs(&self, job: &str) {
        assert!(self.fake.place(job, "h1"), "{job} is placed");
        assert!(self.fake.starting(job), "{job} is starting");
        assert!(self.fake.run(job), "{job} is running");
    }

    /// The host runs `job` to its end with `exit_code`.
    pub(crate) fn host_finishes(&self, job: &str, exit_code: i32) {
        self.host_runs(job);
        assert!(self.fake.finish(job, exit_code), "{job} finishes");
    }

    /// One counter of the executor's page, by its line without the value.
    pub(crate) fn counted(&self, line: &str) -> u64 {
        let mut page = String::new();
        self.metrics.render(&mut page);
        page.lines()
            .find_map(|l| l.strip_prefix(line).and_then(|v| v.trim().parse().ok()))
            .unwrap_or_else(|| panic!("no line {line:?} in\n{page}"))
    }

    /// How many calls of `method` the job service handled.
    pub(crate) fn calls_of(&self, method: &str) -> usize {
        self.fake
            .calls()
            .iter()
            .filter(|call| call_name(call) == method)
            .count()
    }
}

pub(crate) fn call_name(call: &Call) -> &'static str {
    match call {
        Call::SubmitJob(_) => "SubmitJob",
        Call::GetJob(_) => "GetJob",
        Call::ListJobs(_) => "ListJobs",
        Call::CancelJob(_) => "CancelJob",
        Call::GetResult(_) => "GetResult",
        Call::GetLogs(_) => "GetLogs",
    }
}

pub(crate) fn digest_n(n: u32) -> Digest {
    let mut hash = [0_u8; 32];
    hash[..4].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, 1)
}

/// A job as the service reports it once it has ended in `state`, as the answer of a read.
pub(crate) fn job_ended(state: js_proto::v1::JobState) -> kbf_job_client::fake::Fault {
    use prost::Message;
    let job = js_proto::v1::Job {
        state: state.into(),
        ..js_proto::v1::Job::default()
    };
    let answer = js_proto::v1::GetJobResponse { job: Some(job) };
    kbf_job_client::fake::Fault::Answer(bytes::Bytes::from(answer.encode_to_vec()))
}

/// The result of a job that ended in `state` for `reason`, as the answer of a read.
pub(crate) fn result_of(
    state: js_proto::v1::JobState,
    reason: js_proto::v1::ResultReason,
    stderr: &[u8],
) -> kbf_job_client::fake::Fault {
    use prost::Message;
    let result = js_proto::v1::JobResult {
        state: state.into(),
        reason: reason.into(),
        stderr_tail: stderr.to_vec(),
        ..js_proto::v1::JobResult::default()
    };
    let answer = js_proto::v1::GetResultResponse {
        result: Some(result),
    };
    kbf_job_client::fake::Fault::Answer(bytes::Bytes::from(answer.encode_to_vec()))
}

/// The `ErrorInfo` of `reason` in the details of `status`, if it holds one.
pub(crate) fn error_info(
    status: &kbf_proto::google::rpc::Status,
    reason: &str,
) -> Option<kbf_proto::google::rpc::ErrorInfo> {
    use prost::Message;
    status.details.iter().find_map(|any| {
        let info = kbf_proto::google::rpc::ErrorInfo::decode(any.value.as_slice()).ok()?;
        (info.reason == reason).then_some(info)
    })
}
