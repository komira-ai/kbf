//! An executor over a fake job service whose jobs nobody runs, for the tests that need an
//! Execute left open (waiting for its job) and nothing of what a finished job leaves: the
//! stream, the drain and the progress of an open operation. A test that runs jobs to their end
//! uses `support::Cell` and its `direct`, which also fakes the ingest.

#![allow(dead_code, reason = "each test binary uses some of the helpers")]

use std::time::Duration;

use js_proto::v1::{ExecMode, Shape};
use kbf_job_client::fake::{FakeJobServer, Machine};
use kbf_proto::reapi::Digest;
use kbf_server::executor::{
    ActionFacts, Config, Deps, Executor, Ingest, Ingested, InputTree, Inputs, InputsError, Metrics,
    MintError, PackKey, PutDirRoots, RunId, SizeClass, System, UntrustedLimits,
};
use std::sync::Arc;

/// The image every action of [`Idle`] runs in.
const IMAGE: &str =
    "registry.test/app@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The two seams of the executor, answering what an action that is never run needs.
struct Idle;

#[tonic::async_trait]
impl Inputs for Idle {
    async fn action(&self, _action: &Digest) -> Result<ActionFacts, InputsError> {
        let out = "buck-out/v2/art/root/7a3c1e9f0b2d4c68/pkg/__t__/o".to_owned();
        let shape = kbf_server::action_usage::Shape::of(&["cc".to_owned()], &[out]);
        Ok(ActionFacts {
            image: IMAGE.to_owned(),
            timeout: Some(Duration::from_secs(60)),
            system: System::Linux,
            size_class: SizeClass::of_action(&shape),
        })
    }

    async fn tree(&self, _run: RunId) -> Result<String, InputsError> {
        Ok("ab".repeat(32))
    }
}

#[tonic::async_trait]
impl Ingest for Idle {
    async fn mint(
        &self,
        run: RunId,
        _action: &Digest,
        _wall: Duration,
        _tree: InputTree,
    ) -> Result<PackKey, MintError> {
        Ok(PackKey::new(format!(
            "cas/b/{}/{:032x}-0",
            kbf_server::executor::build_id(run),
            0
        )))
    }

    async fn ingest(
        &self,
        _run: RunId,
        _key: &PackKey,
        _action: &Digest,
        _limits: &UntrustedLimits,
    ) -> Ingested {
        Ingested::Unavailable("no job of this executor is run to its end".to_owned())
    }
}

/// A Linux host of 16 cores and 64 GiB that runs containers on `x86_64`.
fn machine() -> Machine {
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

/// An executor over a fake job service with one host. Built on the runtime of the caller, which
/// is the runtime its tasks run on. The job service is returned so that it outlives the
/// executor and a test can read the jobs it was submitted.
pub async fn idle_executor() -> (Executor, Arc<FakeJobServer>) {
    let fake = FakeJobServer::start().await.expect("a loopback port");
    fake.add_host("host-1", machine());
    let fake = Arc::new(fake);
    let mut cfg = Config::new("front-1");
    cfg.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
    cfg.poll = Duration::from_millis(20);
    let seams = Arc::new(Idle);
    let deps = Deps {
        client: fake.client().expect("a job client"),
        ingest: seams.clone(),
        inputs: seams,
        metrics: Arc::new(Metrics::default()),
        store: None,
        recorder: kbf_front::Recorder::off(),
    };
    let executor = Executor::new(cfg, deps).expect("a valid executor config");
    (executor, fake)
}
