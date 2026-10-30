//! The harness: a farm, a lease server, an executor, and fakes for what the executor reads
//! from the rest of the server.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_front::{Cache, Described, Dispatch, Finished, Stage, Submission};
use kbf_lease_client::fake::{FakeLeaseServer, Pool};
use kbf_lease_client::{Client, LeaseId, Outcome, RunId};
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_sched::Request;
use kbf_server::Farm;
use kbf_server::executor::{
    ActionFacts, Clock, Config, Deps, Executor, Ingest, Ingested, Inputs, InputsError, Metrics,
    MintError, Nonce, PackKey, Presign, PresignError, ReadGrant, Secret, UntrustedLimits, build_id,
};
use kbf_types::{ActionKey, LeaseKind, Qos, Resources};
use tokio::sync::watch;

pub const GIB: u64 = 1 << 30;
/// The input bytes the fake grant reports: not a whole GiB, so a sum that drops it shows.
pub const INPUT_BYTES: u64 = 3 * GIB + 4_096;
pub const IMAGE: &str = "docker://registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the executor reads of an action, and the grant of its task.
pub struct FakeInputs {
    pub facts: Mutex<Result<ActionFacts, InputsError>>,
    pub grant: Mutex<Result<ReadGrant, InputsError>>,
    pub looked_up: Mutex<Vec<String>>,
    /// While set, the read of an action waits for `release` (a slow CAS).
    pub hold: std::sync::atomic::AtomicBool,
    pub release: tokio::sync::Notify,
}

impl FakeInputs {
    fn new() -> Self {
        Self {
            facts: Mutex::new(Ok(ActionFacts {
                image: IMAGE.to_owned(),
                timeout: Some(Duration::from_secs(60)),
            })),
            grant: Mutex::new(Ok(ReadGrant {
                url: "https://grant.test:9400".to_owned(),
                token: Secret::new("read-token-0001"),
                input_bytes: Some(INPUT_BYTES),
            })),
            looked_up: Mutex::new(Vec::new()),
            hold: std::sync::atomic::AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
        }
    }
}

#[tonic::async_trait]
impl Inputs for FakeInputs {
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        locked(&self.looked_up).push(action.hash.clone());
        if self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            self.release.notified().await;
        }
        locked(&self.facts).clone()
    }

    async fn read_grant(
        &self,
        _lease: kbf_types::LeaseId,
        _action: &Digest,
        _expires: Duration,
    ) -> Result<ReadGrant, InputsError> {
        locked(&self.grant).clone()
    }
}

/// One url the store minted.
#[derive(Clone, Debug)]
pub struct Minted {
    pub host: String,
    pub key: String,
    pub expires: Duration,
    pub max_bytes: u64,
}

/// A store that signs a url for any key, and takes a `PUT` of a key once.
pub struct FakePresign {
    pub minted: Mutex<Vec<Minted>>,
    pub written: Mutex<BTreeSet<String>>,
    pub fail: Mutex<Option<PresignError>>,
    /// While set, the urls are `http://`: a store that does not serve TLS.
    pub plain: AtomicBool,
}

impl FakePresign {
    fn new() -> Self {
        Self {
            minted: Mutex::new(Vec::new()),
            written: Mutex::new(BTreeSet::new()),
            fail: Mutex::new(None),
            plain: AtomicBool::new(false),
        }
    }

    /// The `PUT` of a task to `url`: refused if the key was written before, as a create-only
    /// store refuses it.
    pub fn put(&self, url: &str) -> Result<(), &'static str> {
        let key = url
            .strip_prefix("https://store.test/")
            .or_else(|| url.strip_prefix("http://store.test/"))
            .and_then(|rest| rest.split('?').next())
            .expect("a url this store minted");
        match locked(&self.written).insert(key.to_owned()) {
            true => Ok(()),
            false => Err("412 the key exists"),
        }
    }

    pub fn keys(&self) -> Vec<String> {
        locked(&self.minted).iter().map(|m| m.key.clone()).collect()
    }
}

#[tonic::async_trait]
impl Presign for FakePresign {
    async fn put_url(
        &self,
        host: &str,
        key: &PackKey,
        expires: Duration,
        max_bytes: u64,
    ) -> Result<Secret, PresignError> {
        if let Some(error) = locked(&self.fail).clone() {
            return Err(error);
        }
        locked(&self.minted).push(Minted {
            host: host.to_owned(),
            key: key.to_string(),
            expires,
            max_bytes,
        });
        let scheme = if self.plain.load(Ordering::Relaxed) {
            "http"
        } else {
            "https"
        };
        Ok(Secret::new(format!(
            "{scheme}://store.test/{key}?sig=s3cr3t"
        )))
    }
}

/// What the frontend makes of a pack: scripted, then the default.
pub struct FakeIngest {
    /// While set, a read waits for `release` (a slow store).
    pub hold: std::sync::atomic::AtomicBool,
    pub release: tokio::sync::Notify,
    /// How many of the next reads panic, as a bug of the real ingest would.
    pub panics: std::sync::atomic::AtomicUsize,
    pub script: Mutex<VecDeque<Ingested>>,
    /// The reason the next mint fails for, if any.
    pub mint_fails: Mutex<Option<MintError>>,
    pub calls: Mutex<Vec<(kbf_types::LeaseId, String, UntrustedLimits)>>,
}

impl FakeIngest {
    fn new() -> Self {
        Self {
            hold: std::sync::atomic::AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            panics: std::sync::atomic::AtomicUsize::new(0),
            script: Mutex::new(VecDeque::new()),
            mint_fails: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
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
}

#[tonic::async_trait]
impl Ingest for FakeIngest {
    async fn mint(
        &self,
        lease: kbf_types::LeaseId,
        _action: &Digest,
        _wall: Duration,
    ) -> Result<PackKey, MintError> {
        match locked(&self.mint_fails).take() {
            Some(why) => Err(why),
            None => Ok(PackKey::new(format!(
                "cas/b/{}/{}-0",
                build_id(lease),
                Nonce::random()
            ))),
        }
    }

    async fn ingest(
        &self,
        lease: kbf_types::LeaseId,
        key: &PackKey,
        _action: &Digest,
        limits: &UntrustedLimits,
    ) -> Ingested {
        locked(&self.calls).push((lease, key.to_string(), *limits));
        if self.hold.load(std::sync::atomic::Ordering::SeqCst) {
            self.release.notified().await;
        }
        let panics = self.panics.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| Some(n.saturating_sub(1)),
        );
        assert!(panics == Ok(0), "the ingest broke");
        locked(&self.script)
            .pop_front()
            .unwrap_or_else(|| Self::ran(0))
    }
}

/// Holds tokio's paused clock where it is. The paused clock jumps to its next timer whenever
/// the runtime waits, which a client's timeout, a retry delay of the farm and a socket being
/// served all do; a blocking task in flight stops the jump. The tests move the clock by hand,
/// and so assert on what happened, never on how long it took.
pub struct ClockHold {
    release: Option<std::sync::mpsc::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ClockHold {
    pub fn new() -> Self {
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

/// A farm, a lease server of `hosts`, and an executor that holds them.
pub struct Rig {
    pub fake: FakeLeaseServer,
    pub api: Client,
    pub farm: Arc<Farm>,
    pub exec: Executor,
    pub cfg: Config,
    pub inputs: Arc<FakeInputs>,
    pub presign: Arc<FakePresign>,
    pub ingest: Arc<FakeIngest>,
    pub metrics: Arc<Metrics>,
    _hold: ClockHold,
}

/// The farm leases an action makes, by what the test calls it.
pub struct Caller {
    pub tag: u8,
    pub stage: watch::Receiver<Stage>,
}

impl Caller {
    pub fn finished(&self) -> Option<Finished> {
        match &*self.stage.borrow() {
            Stage::Done(finished) => Some(finished.clone()),
            _ => None,
        }
    }
}

/// A config whose longest run (3800 s of wall time) outlasts the 3600 s the lease server grants
/// at most: the lease is renewed before every post, and cuts the run it is posted with.
pub fn long_runs(cfg: &mut Config) {
    cfg.max_run = Duration::from_secs(3_500);
    cfg.default_timeout = Duration::from_secs(3_500);
    cfg.lease_ttl = Duration::from_secs(4 * 3600);
    cfg.max_wall = Duration::from_secs(12 * 3600);
    cfg.drain_before_end = Duration::from_secs(3 * 3600);
}

pub fn digest_of(tag: u8) -> kbf_types::Digest {
    kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

impl Rig {
    /// A rig over a pool of `hosts` of 2 cores and 16 GiB, held by an executor of `held`
    /// hosts with `adjust` applied to its config.
    pub async fn start(hosts: &[&str], held: usize, adjust: impl FnOnce(&mut Config)) -> Self {
        Self::shaped(hosts, held, 16, adjust).await
    }

    /// As [`Rig::start`], over hosts of `gib` GiB of memory.
    pub async fn shaped(
        hosts: &[&str],
        held: usize,
        gib: u64,
        adjust: impl FnOnce(&mut Config),
    ) -> Self {
        Self::built(hosts, held, gib, false, adjust).await
    }

    /// As [`Rig::shaped`]; with `prepare` a lease is `202 pending` until its host is ready.
    pub async fn built(
        hosts: &[&str],
        held: usize,
        gib: u64,
        prepare: bool,
        adjust: impl FnOnce(&mut Config),
    ) -> Self {
        Self::pooled(hosts, held, gib, |pool| pool.prepare = prepare, adjust).await
    }

    /// As [`Rig::built`], over a pool `shape` changes.
    pub async fn pooled(
        hosts: &[&str],
        held: usize,
        gib: u64,
        shape: impl FnOnce(&mut Pool),
        adjust: impl FnOnce(&mut Config),
    ) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::INFO)
            .try_init();
        let hold = ClockHold::new();
        let mut pool = Pool::new("farm-linux", hosts, 2_000, gib * GIB);
        pool.capabilities
            .insert("arch".to_owned(), "x86_64".to_owned());
        shape(&mut pool);
        let fake = FakeLeaseServer::start(pool).await;
        let api = fake.client().expect("a client");
        let mut cfg = Config::new("front-1", held, "farm-linux", 2_000, gib * GIB);
        cfg.slots = 2;
        cfg.insecure_argv_secrets = true;
        adjust(&mut cfg);
        let farm = Arc::new(Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ));
        let (inputs, presign, ingest) = (
            Arc::new(FakeInputs::new()),
            Arc::new(FakePresign::new()),
            Arc::new(FakeIngest::new()),
        );
        let metrics = Arc::new(Metrics::default());
        let exec = Self::executor(&fake, &farm, &cfg, &inputs, &presign, &ingest, &metrics);
        Self {
            fake,
            api,
            farm,
            exec,
            cfg,
            inputs,
            presign,
            ingest,
            metrics,
            _hold: hold,
        }
    }

    fn executor(
        fake: &FakeLeaseServer,
        farm: &Arc<Farm>,
        cfg: &Config,
        inputs: &Arc<FakeInputs>,
        presign: &Arc<FakePresign>,
        ingest: &Arc<FakeIngest>,
        metrics: &Arc<Metrics>,
    ) -> Executor {
        let deps = Deps {
            farm: Arc::clone(farm),
            client: fake.client().expect("a client"),
            ingest: ingest.clone(),
            presign: presign.clone(),
            inputs: inputs.clone(),
            metrics: Arc::clone(metrics),
        };
        Executor::new(cfg.clone(), deps, Clock::from_unix_ms(fake.now_ms()))
            .expect("a valid config")
    }

    /// A new executor over the same farm and lease server, as a restarted frontend has: the
    /// same frontend id, and a process of its own.
    pub fn restarted_executor(&self) -> Executor {
        self.other_executor(&self.cfg.frontend)
    }

    /// A new executor over the same farm and lease server, of the frontend `frontend` and a
    /// process of its own.
    pub fn other_executor(&self, frontend: &str) -> Executor {
        let mut cfg = self.cfg.clone();
        cfg.frontend = frontend.to_owned();
        cfg.instance = kbf_server::executor::Nonce::random().to_string();
        Self::executor(
            &self.fake,
            &self.farm,
            &cfg,
            &self.inputs,
            &self.presign,
            &self.ingest,
            &self.metrics,
        )
    }

    /// Lets pass the time an executor watches the leases of an earlier process for a renewal.
    /// Nothing is stepped: the process that made them is gone, and renews nothing.
    pub async fn watch_over(&self) {
        self.advance(self.cfg.adopt_watch() + Duration::from_secs(1))
            .await;
    }

    /// One step of the executor and of the farm, with the tasks they spawn run to rest.
    pub async fn pump(&mut self) {
        for _ in 0..4 {
            self.exec.step().await;
            settle().await;
            self.farm.tick().await;
            settle().await;
        }
    }

    /// Time passes: tokio's clock, the lease server's, and the farm's with the first.
    pub async fn advance(&self, by: Duration) {
        tokio::time::advance(by).await;
        self.fake.advance(by);
    }

    /// `seconds` pass in steps of 5 s, the executor and the farm stepping after each.
    pub async fn elapse(&mut self, seconds: u64) {
        for _ in 0..seconds.div_ceil(5) {
            self.advance(Duration::from_secs(5)).await;
            self.pump().await;
        }
    }

    /// Submits the action of `tag`, booking one core and one GiB.
    pub fn submit(&self, tag: u8) -> Caller {
        self.submit_kind(tag, LeaseKind::Action)
    }

    /// Submits the action of `tag` as a lease of `kind`.
    pub fn submit_kind(&self, tag: u8, kind: LeaseKind) -> Caller {
        let request = Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: digest_of(tag),
            }
            .into(),
            qos: Qos::Ci,
            kind,
            resources: Resources::new(1_000, GIB),
            hermetic: true,
            do_not_cache: false,
            whole_worker: false,
            needs: kbf_caps::Request::default(),
        };
        let ticket = self
            .farm
            .submit(Submission {
                request,
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
        Caller {
            tag,
            stage: ticket.stage,
        }
    }

    /// Every run of every lease the lease server holds, with its lease and its arguments.
    pub async fn runs(&self) -> Vec<Posted> {
        let mut posted = Vec::new();
        for lease in self.api.leases().await.expect("leases") {
            for run in self.api.runs(lease.id).await.expect("runs") {
                let args = self.fake.run_args(&run.run_id).unwrap_or_default();
                posted.push(Posted {
                    lease: lease.id,
                    host: lease.host.clone().unwrap_or_default(),
                    run: run.run_id,
                    args,
                });
            }
        }
        posted
    }

    /// The runs queued for the action of `tag`.
    pub async fn runs_of(&self, tag: u8) -> Vec<Posted> {
        let want = format!("{}/{}", hex(tag), tag);
        self.runs()
            .await
            .into_iter()
            .filter(|p| p.arg("--action") == Some(want.as_str()))
            .collect()
    }

    /// The host runs `run` to the end with `outcome`, and stops listing it.
    pub fn host_finishes(&self, run: &RunId, outcome: Outcome, exit_code: Option<i32>) {
        self.fake.run_started(run);
        assert!(
            self.fake
                .finish_run(run, outcome, exit_code, Some("detail"))
        );
    }

    /// As [`Rig::host_finishes`], with the detail the host gives (a refusal code, for a start).
    pub fn host_finishes_with(
        &self,
        run: &RunId,
        outcome: Outcome,
        exit_code: Option<i32>,
        detail: &str,
    ) {
        self.fake.run_started(run);
        assert!(self.fake.finish_run(run, outcome, exit_code, Some(detail)));
    }

    pub fn host_ends(&self, run: &RunId) {
        assert!(self.fake.end_run(run));
    }

    /// One step of the executor while `by` passes on every clock after it has begun and
    /// waited as long as it will. True if the step had ended by then; a step that had not is
    /// given 31 s more (the client's own request timeout) so that it ends, and the answer is
    /// false.
    pub async fn step_through(&mut self, by: Duration) -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = AtomicBool::new(false);
        let (exec, fake, inputs) = (&mut self.exec, &self.fake, &self.inputs);
        let stepping = async {
            exec.step().await;
            done.store(true, Ordering::SeqCst);
        };
        let passing = async {
            settle().await;
            tokio::time::advance(by).await;
            fake.advance(by);
            settle().await;
            let in_time = done.load(Ordering::SeqCst);
            if !in_time {
                // Whatever the step waits on is let go, so that a failure fails and does not hang.
                inputs.hold.store(false, Ordering::SeqCst);
                inputs.release.notify_one();
                tokio::time::advance(Duration::from_secs(31)).await;
                fake.advance(Duration::from_secs(31));
            }
            in_time
        };
        tokio::join!(stepping, passing).1
    }

    /// Runs the executor until `caller` is done, or panics after `steps` of 5 s.
    pub async fn until_done(&mut self, caller: &Caller, steps: usize) -> Finished {
        for _ in 0..steps {
            if let Some(finished) = caller.finished() {
                return finished;
            }
            self.advance(Duration::from_secs(5)).await;
            self.pump().await;
        }
        panic!(
            "action {} not done: {:?}\nhosts {:?}\nrequests {:#?}",
            caller.tag,
            caller.stage.borrow(),
            self.exec.hosts(),
            self.fake.requests()
        );
    }

    pub fn requests(&self) -> Vec<String> {
        self.fake.requests()
    }

    /// What the executor counts, as `GET /metrics` shows it.
    pub fn counted(&self) -> String {
        let mut text = String::new();
        self.metrics.render(&mut text);
        text
    }

    /// Every action the rig is asked about runs for `timeout`.
    pub fn set_timeout(&self, timeout: Duration) {
        locked(&self.inputs.facts)
            .as_mut()
            .expect("ok facts")
            .timeout = Some(timeout);
    }
}

pub fn hex(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

/// Lets the tasks the executor and the farm spawned run to rest. The farm's cache lingers
/// 2 ms before it writes a batch, and the clock is held, so the clock moves 3 ms a round: the
/// tests' own clock is `Rig::advance` and `Rig::elapse`.
pub async fn settle() {
    for _ in 0..3 {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(3)).await;
    }
}

/// A run as the lease server holds it.
#[derive(Clone, Debug)]
pub struct Posted {
    pub lease: LeaseId,
    pub host: String,
    pub run: RunId,
    pub args: Vec<String>,
}

impl Posted {
    /// The value after `flag` in the task tool's arguments.
    pub fn arg(&self, flag: &str) -> Option<&str> {
        let at = self.args.iter().position(|a| a == flag)?;
        self.args.get(at + 1).map(String::as_str)
    }
}

/// How many of `requests` are exactly `request`.
pub fn count(requests: &[String], request: &str) -> usize {
    requests.iter().filter(|r| *r == request).count()
}

/// How many of `requests` are `method` on a path under `/v1/leases/` that ends `tail`.
pub fn count_tail(requests: &[String], method: &str, tail: &str) -> usize {
    let prefix = format!("{method} /v1/leases/");
    requests
        .iter()
        .filter(|r| r.starts_with(&prefix) && r.ends_with(tail))
        .count()
}
