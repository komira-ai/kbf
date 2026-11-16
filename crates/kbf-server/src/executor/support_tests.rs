//! A worker, a farm and a job service to call the executor's private parts with: for the
//! arms the farm-level tests (`tests/executor`) cannot reach, because the farm never sends
//! them or the job service's fake cannot make them happen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_proto::v1::{ExecMode, Shape};
use kbf_front::Cache;
use kbf_job_client::fake::{FakeJobServer, Machine};
use kbf_proto::reapi::Digest;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::LeaseId;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::Farm;
use crate::executor::host::HostWorker;
use crate::executor::rows::{ExecRows, Held};
use crate::executor::share::Share;
use crate::executor::traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, MintError, Presign, PresignError,
    ReadGrant, UntrustedLimits,
};
use crate::executor::{Config, Ctx, Deps, Ownership, PackKey, Secret, Twin, build_id};

pub(super) const GIB: u64 = 1 << 30;

/// The one host of the fake job service.
pub(super) const HOST: &str = "host-1";

/// Fakes that answer at once and never fail.
struct Easy;

#[tonic::async_trait]
impl Ingest for Easy {
    async fn mint(&self, lease: LeaseId, _: &Digest, _: Duration) -> Result<PackKey, MintError> {
        Ok(PackKey::new(format!("cas/b/{}/aa-0", build_id(lease))))
    }

    async fn ingest(&self, _: LeaseId, _: &PackKey, _: &Digest, _: &UntrustedLimits) -> Ingested {
        Ingested::Unavailable("not read here".to_owned())
    }
}

#[tonic::async_trait]
impl Presign for Easy {
    async fn put_url(&self, key: &PackKey, _: Duration, _: u64) -> Result<Secret, PresignError> {
        Ok(Secret::new(format!("https://store.test/{key}")))
    }
}

#[tonic::async_trait]
impl Inputs for Easy {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/app@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
            system: crate::executor::System::Linux,
        })
    }

    async fn read_grant(
        &self,
        _: LeaseId,
        _: &Digest,
        _: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Ok(ReadGrant {
            url: "https://grant.test:9400".to_owned(),
            token: Secret::new("token"),
        })
    }
}

/// A Linux host of 16 cores, 64 GiB and 1 TiB of disk, running containers: it can run every
/// job these tests submit.
pub(super) fn machine() -> Machine {
    Machine {
        shape: Shape {
            cpu_millis: 16_000,
            memory_bytes: 64 * GIB,
            gpus: 0,
            gpu_class: String::new(),
            disk_bytes: Some(1 << 40),
        },
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        exec: ExecMode::Container,
    }
}

/// The tokio clock is held while the harness lives: see `tests/executor/rig.rs`.
pub(super) struct Harness {
    pub(super) fake: FakeJobServer,
    pub(super) farm: Arc<Farm>,
    pub(super) cfg: Config,
    pub(super) deps: Deps,
    pub(super) twin: Twin,
    /// The process's rows and the store under them, for a harness of shared ownership.
    pub(super) share: Option<Share>,
    pub(super) rig: Option<Rig>,
    release: Option<std::sync::mpsc::Sender<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.release.take().iter().for_each(|release| {
            let _ = release.send(());
        });
    }
}

impl Harness {
    pub(super) async fn new() -> Self {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        drop(tokio::task::spawn_blocking(move || {
            let _ = hold.recv();
        }));
        let fake = FakeJobServer::start().await.expect("a loopback port");
        fake.add_host(HOST, machine());
        let farm = Arc::new(Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ));
        let mut cfg = Config::new("front", 1, 2_000, 16 * GIB);
        cfg.slots = 2;
        cfg.insecure_argv_secrets = true;
        let deps = Deps {
            farm: Arc::clone(&farm),
            client: fake.client().expect("a client"),
            ingest: Arc::new(Easy),
            presign: Arc::new(Easy),
            inputs: Arc::new(Easy),
            metrics: Arc::default(),
            store: None,
        };
        Self {
            fake,
            farm,
            cfg,
            deps,
            twin: Twin::default(),
            share: None,
            rig: None,
            release: Some(release),
        }
    }

    pub(super) fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            twin: &self.twin,
            share: self.share.as_ref(),
        }
    }

    /// The first virtual worker, taking its messages from the returned sender instead of the
    /// farm.
    pub(super) fn worker(
        &self,
    ) -> (
        HostWorker,
        mpsc::UnboundedSender<Result<kbf_proto::worker::ServerMessage, tonic::Status>>,
    ) {
        let mut worker = HostWorker::start(&self.ctx(), 0, Instant::now());
        let (messages, inbox) = mpsc::unbounded_channel();
        worker.replace_inbox(inbox);
        (worker, messages)
    }
}

/// The state store of a shared harness: an in-memory store whose clock a test moves, and the
/// handle through which a test makes its calls fail.
pub(super) struct Rig {
    pub(super) fake: Arc<Fake>,
    pub(super) flaky: Arc<Flaky>,
}

/// A store that fails on demand: while a flag is set, writes (or reads) answer a
/// [`StoreError`] and change nothing; otherwise it is the store underneath.
pub(super) struct Flaky {
    inner: Arc<Fake>,
    writes: AtomicBool,
    reads: AtomicBool,
}

impl Flaky {
    /// Whether the transactions that follow fail.
    pub(super) fn fail_writes(&self, fail: bool) {
        self.writes.store(fail, Ordering::SeqCst);
    }

    /// Whether the reads (`get`, scans) that follow fail.
    pub(super) fn fail_reads(&self, fail: bool) {
        self.reads.store(fail, Ordering::SeqCst);
    }

    fn down(flag: &AtomicBool) -> Result<(), StoreError> {
        match flag.load(Ordering::SeqCst) {
            true => Err(StoreError::Failed("injected".to_owned())),
            false => Ok(()),
        }
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        Self::down(&self.writes)?;
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        Self::down(&self.reads)?;
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        Self::down(&self.reads)?;
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

impl Rig {
    /// The rows as a process of the group other than the harness's sees them: over the store
    /// underneath, which never fails.
    pub(super) fn rows(&self) -> ExecRows {
        ExecRows::new(Arc::clone(&self.fake) as Arc<dyn Store>)
    }
}

/// The instance of the process a shared harness stands for. Its first eight characters name
/// its workers.
pub(super) const INSTANCE: &str = "inst0001-process";

impl Harness {
    /// A harness of shared ownership over an in-memory store. Nothing is written yet: the
    /// liveness row is renewed by [`Harness::renew`], and the workers are opened by
    /// [`Harness::open`].
    pub(super) async fn shared() -> Self {
        let mut harness = Self::new().await;
        harness.cfg.ownership = Ownership::Shared;
        harness.cfg.instance = INSTANCE.to_owned();
        let clock = Arc::new(ManualClock::default());
        let fake = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
        let flaky = Arc::new(Flaky {
            inner: Arc::clone(&fake),
            writes: AtomicBool::new(false),
            reads: AtomicBool::new(false),
        });
        let store: Arc<dyn Store> = Arc::clone(&flaky) as Arc<dyn Store>;
        harness.deps.store = Some(Arc::clone(&store));
        harness.share = Some(Share::new(&harness.cfg, store, Instant::now()));
        harness.rig = Some(Rig { fake, flaky });
        harness
    }

    pub(super) fn rig(&self) -> &Rig {
        self.rig.as_ref().expect("a shared harness")
    }

    /// The process renews its liveness row now, with a check of the rows of `held`.
    pub(super) fn renew(&mut self, held: &[Held], draining: bool) {
        let share = self.share.as_mut().expect("a shared harness");
        share.beat(&self.cfg, held, draining, Instant::now());
    }

    /// Opens the workers `cfg.hosts` asks for that `workers` does not hold yet.
    pub(super) fn open(&self, workers: &mut Vec<HostWorker>) {
        let share = self.share.as_ref().expect("a shared harness");
        share.open(&self.ctx(), workers, Instant::now());
    }

    /// The first worker of a shared process, its liveness row renewed and its row written,
    /// taking its messages from the returned sender instead of the farm.
    pub(super) fn shared_worker(
        &mut self,
    ) -> (
        HostWorker,
        mpsc::UnboundedSender<Result<kbf_proto::worker::ServerMessage, tonic::Status>>,
    ) {
        self.renew(&[], false);
        let mut workers = Vec::new();
        self.open(&mut workers);
        let mut worker = workers.pop().expect("a worker was opened");
        let (messages, inbox) = mpsc::unbounded_channel();
        worker.replace_inbox(inbox);
        (worker, messages)
    }
}

/// A `Start` of the farm lease `term`.`seq`, naming the registration's heartbeat.
pub(super) fn start(term: u64, seq: u64) -> kbf_proto::worker::Start {
    kbf_proto::worker::Start {
        lease_id: Some(kbf_proto::worker::LeaseId { term, seq }),
        kind: "action".to_owned(),
        action_digest: Some(Digest {
            hash: "cd".repeat(32),
            size_bytes: 3,
        }),
        millicpus: 1_000,
        memory_bytes: GIB,
        heartbeat_seq: 0,
        valid_for_ms: 14_000,
        ..Default::default()
    }
}

/// The log of a test, as text.
#[derive(Clone, Default)]
pub(super) struct Log(Arc<Mutex<Vec<u8>>>);

pub(super) struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    pub(super) fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The value of the field `name` of the first log line that has it, up to the next space.
    pub(super) fn field(&self, name: &str) -> Option<String> {
        let text = self.text();
        let tail = text.split_once(&format!("{name}="))?.1;
        Some(tail.split_whitespace().next()?.to_owned())
    }

    /// Runs `f` with everything it logs written here.
    pub(super) fn capture<T>(&self, f: impl FnOnce() -> T) -> T {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f)
    }

    /// Writes everything the current thread logs here until the guard is dropped: for an
    /// async test on one thread.
    pub(super) fn install(&self) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }
}
