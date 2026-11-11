//! A worker, a farm and a job service to call the executor's private parts with: for the
//! arms the farm-level tests (`tests/executor`) cannot reach, because the farm never sends
//! them or the job service's fake cannot make them happen.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_proto::v1::{ExecMode, Shape};
use kbf_front::Cache;
use kbf_job_client::fake::{FakeJobServer, Machine};
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::Farm;
use crate::executor::host::HostWorker;
use crate::executor::traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, MintError, Presign, PresignError,
    ReadGrant, UntrustedLimits,
};
use crate::executor::{Config, Ctx, Deps, PackKey, Secret, Twin, build_id};

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
        };
        Self {
            fake,
            farm,
            cfg,
            deps,
            twin: Twin::default(),
            release: Some(release),
        }
    }

    pub(super) fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            twin: &self.twin,
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
