//! A worker, a farm and a lease server to call the executor's private parts with: for the
//! arms the farm-level tests (`tests/executor`) cannot reach, because the farm never sends
//! them or the lease server's fake cannot make them happen.

use std::sync::Arc;
use std::time::Duration;

use kbf_front::Cache;
use kbf_lease_client::fake::{FakeLeaseServer, Pool};
use kbf_lease_client::{AcquireRequest, Acquired, Resources};
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
use crate::executor::{Bench, Clock, Config, Ctx, Deps, PackKey, Secret, Strays, build_id};

pub(super) const GIB: u64 = 1 << 30;

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
    async fn put_url(
        &self,
        _: &str,
        key: &PackKey,
        _: Duration,
        _: u64,
    ) -> Result<Secret, PresignError> {
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
            input_bytes: Some(3 * GIB),
        })
    }
}

/// The tokio clock is held while the harness lives: see `tests/executor/rig.rs`.
pub(super) struct Harness {
    pub(super) fake: FakeLeaseServer,
    pub(super) farm: Arc<Farm>,
    pub(super) cfg: Config,
    pub(super) deps: Deps,
    pub(super) clock: Clock,
    pub(super) strays: Strays,
    pub(super) bench: Bench,
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
        let fake = FakeLeaseServer::start(Pool::new("pool", &["node1"], 2_000, 16 * GIB)).await;
        let farm = Arc::new(Farm::new_legacy(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        ));
        let mut cfg = Config::new("front", 1, "pool", 2_000, 16 * GIB);
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
        let clock = Clock::from_unix_ms(fake.now_ms());
        Self {
            fake,
            farm,
            cfg,
            deps,
            clock,
            strays: Strays::default(),
            bench: Bench::default(),
            release: Some(release),
        }
    }

    pub(super) fn ctx(&self) -> Ctx<'_> {
        Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            clock: &self.clock,
            strays: &self.strays,
            bench: &self.bench,
        }
    }

    /// A worker over a lease of the fake's one host, taking its messages from the returned
    /// sender instead of the farm.
    pub(super) async fn worker(
        &self,
    ) -> (
        HostWorker,
        mpsc::UnboundedSender<Result<kbf_proto::worker::ServerMessage, tonic::Status>>,
    ) {
        let resources = Resources {
            cpu_millis: 2_000,
            memory_bytes: 16 * GIB,
            gpus: 0,
            gpu_class: None,
        };
        let request = AcquireRequest::new("pool", resources, self.cfg.max_wall);
        let Acquired::Granted(granted) = self.deps.client.acquire(&request).await.expect("a lease")
        else {
            panic!("the lease is booked, not granted");
        };
        let now = Instant::now();
        let ends = (now + self.cfg.lease_ttl, now + self.cfg.max_wall);
        let mut worker = HostWorker::start(&self.ctx(), granted.id, granted.host, ends, now);
        let (messages, inbox) = mpsc::unbounded_channel();
        worker.replace_inbox(inbox);
        (worker, messages)
    }
}

/// A `Start` of the action of `tag`, naming the registration's heartbeat.
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
