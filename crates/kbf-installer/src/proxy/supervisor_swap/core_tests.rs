//! The supervisor behind the update API's core: the API stages and hands over, the supervisor
//! swaps, and the status the client reads is the supervisor's. The children, the proxy's gate
//! and the root half are the rig's; the core and its request map are the real ones.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper::StatusCode;

use super::rig::Rig;
use super::{ExpandOnlyDefault, SwapSupervisor};
use crate::CommitId;
use crate::proxy::api::core::{Config, Core, Fault, NoCrash, Parts, Reply};
use crate::proxy::api::idempotency::Idempotency;
use crate::proxy::api::model::{StatusBody, UpdateRequest};
use crate::proxy::api::traits::{
    InstallerState, InstallerStatus, OrderError, OrderWriter, RestartPending, StagedWire,
    StatusError, StatusReader, StubGate, SystemClock,
};
use crate::proxy::model::{Locator, Phase, Slot, Step, Timing, Verb};
use crate::proxy::runtime::RequestQueue;
use crate::testkit::{A, B, c};

#[derive(Clone)]
struct Status(Arc<Mutex<InstallerStatus>>);

impl StatusReader for Status {
    fn read(&self) -> Result<InstallerStatus, StatusError> {
        Ok(self.0.lock().unwrap().clone())
    }
}

#[derive(Clone, Default)]
struct Orders(Arc<Mutex<Vec<Locator>>>);

impl OrderWriter for Orders {
    fn write(
        &mut self,
        verb: Verb,
        commit: &CommitId,
        window_s: u32,
    ) -> Result<Locator, OrderError> {
        let mut written = self.0.lock().unwrap();
        let n = written.len() as u64 + 1;
        let locator = Locator {
            order_id: 100 + n,
            epoch: 1,
            generation: n,
            verb,
            commit: commit.clone(),
            window_s,
        };
        written.push(locator.clone());
        Ok(locator)
    }
}

struct NoRestart;

impl RestartPending for NoRestart {
    fn restart_pending(&self) -> bool {
        false
    }
}

struct Api {
    core: Core,
    rig: Arc<Rig>,
    status: Status,
    orders: Orders,
}

fn status_of(state: InstallerState, staged: Option<&str>) -> InstallerStatus {
    InstallerStatus {
        installed_commit: Some(c(A)),
        staged_commit: staged.map(c),
        state,
        staged: staged.map(|_| StagedWire {
            protocol: 7,
            min_protocol: 5,
            state_schema: 3,
            profile: None,
        }),
        ..InstallerStatus::default()
    }
}

/// A core over a real supervisor: the migrator is the default one, over the rig's root half.
fn api(name: &str, timing: Timing, host_schema: u32) -> Api {
    let dir = kbf_host::testkit::scratch(name);
    let rig = Rig::new(Some(dir.clone()));
    let requests = Arc::new(RequestQueue::new());
    let mut parts = rig.parts(&requests, |_| {});
    parts.migrator = Arc::new(ExpandOnlyDefault::new(
        rig.clone(),
        5,
        Box::new(move || Ok(host_schema)),
    ));
    let mut config = Rig::config(dir.clone());
    config.timing = timing;
    let status = Status(Arc::new(Mutex::new(status_of(InstallerState::Idle, None))));
    let orders = Orders::default();
    let core = Core::new(Parts {
        supervisor: Box::new(SwapSupervisor::new(parts, config)),
        orders: Box::new(orders.clone()),
        status: Box::new(status.clone()),
        compat: Box::new(StubGate),
        clock: Arc::new(SystemClock::new()),
        restart: Box::new(NoRestart),
        requests: Idempotency::open(&dir),
        hook: Box::new(NoCrash),
        config: Config::default(),
    });
    Api {
        core,
        rig,
        status,
        orders,
    }
}

fn request() -> UpdateRequest {
    UpdateRequest {
        commit: c(B),
        request_id: "req-1".into(),
        nodes: true,
        rollback: false,
    }
}

impl Api {
    fn body(&mut self) -> StatusBody {
        match self.core.status() {
            Reply::Status(b) => *b,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    /// POST, then the root half stages the release.
    fn accept_and_stage(&mut self) {
        match self.core.post(request()) {
            Ok(Reply::Update { phase, .. }) => assert_eq!(phase, Phase::Accepted),
            other => panic!("expected an accepted update, got {other:?}"),
        }
        *self.status.0.lock().unwrap() = status_of(InstallerState::Staged, Some(B));
    }

    /// Read the status until `done` holds for it.
    fn until(&mut self, what: &str, done: impl Fn(&StatusBody) -> bool) -> StatusBody {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let body = self.body();
            if done(&body) {
                return body;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}: {body:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn a_staged_release_is_handed_over_and_the_client_reads_the_swap_to_done() {
    let mut api = api("sw-core-done", super::rig::quick(), 3);
    api.accept_and_stage();
    let body = api.until("done", |b| b.phase == Some(Phase::Done));
    assert_eq!(body.active_slot, Some(Slot::B));
    assert_eq!(body.from, Some(c(A)));
    assert_eq!(body.to, Some(c(B)));
    assert_eq!(body.failure, None);
    let orders = api.orders.0.lock().unwrap().clone();
    assert_eq!(
        orders.iter().map(|o| o.verb).collect::<Vec<_>>(),
        [Verb::FrontendUpdate],
        "the API wrote one order; the swap's confirm goes to the root half's own port"
    );
    assert_eq!(api.rig.count("order confirm"), 1);
    assert_eq!(api.rig.count("spawn"), 1);
}

#[test]
fn a_migrator_refusal_ends_the_update_failed_at_migrate_with_no_candidate() {
    // The staged release needs state schema 3 and the host runs 2: the default migrator
    // will not migrate.
    let mut api = api("sw-core-migrate", super::rig::quick(), 2);
    api.accept_and_stage();
    let body = api.until("failed", |b| b.phase == Some(Phase::Failed));
    let failure = body.failure.expect("a failure");
    assert_eq!(failure.step, Step::Migrate);
    assert!(
        failure.reason.starts_with("not_additive:"),
        "{}",
        failure.reason
    );
    assert_eq!(body.active_slot, Some(Slot::A));
    assert_eq!(api.rig.count("spawn"), 0, "no candidate was started");
    assert_eq!(api.rig.count("hold"), 0);
    assert_eq!(
        api.rig.count("order revert"),
        1,
        "the staged release is dropped"
    );
}

#[test]
fn abort_through_the_api_ends_a_swap_that_has_not_switched() {
    let mut api = api("sw-core-abort", super::rig::quick(), 3);
    api.rig.knobs(|k| k.ready = false);
    api.accept_and_stage();
    api.until("starting", |b| b.phase == Some(Phase::Starting));
    match api.core.abort() {
        Ok(Reply::Update { phase, .. }) => assert_eq!(phase, Phase::Starting),
        other => panic!("expected an abort answer, got {other:?}"),
    }
    let body = api.until("failed", |b| b.phase == Some(Phase::Failed));
    assert_eq!(body.failure.map(|f| f.step), Some(Step::Abort));
    assert_eq!(body.active_slot, Some(Slot::A));
    assert!(api.rig.alive(Slot::A));
    assert_eq!(api.rig.count("hold"), 0);
}

#[test]
fn a_second_update_is_refused_while_one_is_swapping() {
    let mut api = api("sw-core-open", super::rig::quick(), 3);
    api.rig.knobs(|k| k.ready = false);
    api.accept_and_stage();
    api.until("starting", |b| b.phase == Some(Phase::Starting));
    let mut second = request();
    second.request_id = "req-2".into();
    match api.core.post(second) {
        Err(Fault::Api(e)) => assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "update_open")),
        other => panic!("expected update_open, got {other:?}"),
    }
    api.rig.stop(true);
}

#[test]
fn rollback_after_the_switch_is_refused_with_the_reason_and_the_swap_goes_on() {
    let timing = Timing {
        bake: Duration::from_secs(30),
        ..super::rig::quick()
    };
    let mut api = api("sw-core-rollback", timing, 3);
    api.accept_and_stage();
    api.until("baking", |b| b.phase == Some(Phase::Baking));
    match api.core.rollback() {
        Err(Fault::Api(e)) => {
            assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "refused"));
            assert!(e.message.contains("not supported"), "{}", e.message);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(api.body().phase, Some(Phase::Baking));
    api.rig.stop(true);
}
