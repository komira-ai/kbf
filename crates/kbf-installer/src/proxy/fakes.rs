//! Test doubles for the seams of the update API, and a rig that assembles them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use hyper::{Request, Response};

use super::auth::{Secret, TokenHasher, TokenSource};
use super::core::{Config, Core, Crash, CrashHook, CrashPoint, Parts};
use super::idempotency::Idempotency;
use super::model::{Locator, NewChild, OldChild, OrderVerb, Phase, Slot, StagedFacts, UpdateId};
use super::traits::{
    Clock, Compat, CompatGate, ForwardError, Forwarder, InstallerState, InstallerStatus,
    OrderError, OrderWriter, RestartMarker, Retired, RunningUpdate, StatusError, StatusReader,
    Supervisor, SupervisorError, SupervisorStatus, UpdateSpec,
};
use crate::CommitId;
use crate::testkit::{A, B, C, c};

pub const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

pub struct FakeClock(pub AtomicU64);

impl Clock for FakeClock {
    fn mono_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn wall_s(&self) -> u64 {
        1_800_000_000 + self.0.load(Ordering::SeqCst) / 1000
    }
}

pub struct FakeTokens(pub Mutex<Option<String>>);

impl TokenSource for FakeTokens {
    fn token(&self) -> Option<Secret> {
        self.0.lock().unwrap().clone().map(Secret::new)
    }
}

#[derive(Clone, Default)]
pub struct CountingHasher(pub Arc<AtomicUsize>);

impl TokenHasher for CountingHasher {
    fn digest(&self, data: &[u8]) -> [u8; 32] {
        self.0.fetch_add(1, Ordering::SeqCst);
        super::auth::Sha256Hasher.digest(data)
    }
}

#[derive(Clone)]
pub struct FakeStatus(pub Arc<Mutex<Result<InstallerStatus, String>>>);

impl StatusReader for FakeStatus {
    fn read(&self) -> Result<InstallerStatus, StatusError> {
        self.0.lock().unwrap().clone().map_err(StatusError)
    }
}

#[derive(Clone, Default)]
pub struct FakeOrders {
    pub written: Arc<Mutex<Vec<Locator>>>,
    pub fail: Arc<Mutex<bool>>,
}

impl OrderWriter for FakeOrders {
    fn write(
        &mut self,
        verb: OrderVerb,
        commit: &CommitId,
        window_s: u32,
    ) -> Result<Locator, OrderError> {
        if *self.fail.lock().unwrap() {
            return Err(OrderError("orders/ is not writable".to_owned()));
        }
        let mut written = self.written.lock().unwrap();
        let n = written.len() as u64 + 1;
        let loc = Locator {
            order_id: 100 + n,
            epoch: 1,
            generation: n,
            verb,
            commit: commit.clone(),
            window_s,
        };
        written.push(loc.clone());
        Ok(loc)
    }
}

#[derive(Default)]
pub struct SupState {
    pub status: SupervisorStatus,
    pub starts: Vec<UpdateSpec>,
    pub calls: Vec<&'static str>,
    pub start_fails: bool,
}

#[derive(Clone, Default)]
pub struct FakeSupervisor(pub Arc<Mutex<SupState>>);

impl FakeSupervisor {
    /// Puts an update in the supervisor at `phase`.
    pub fn set(&self, phase: Phase, old: Option<OldChild>, nodes_updated: u32) {
        self.0.lock().unwrap().status.update = Some(RunningUpdate {
            update_id: UpdateId("upd-9".to_owned()),
            from: c(A),
            to: c(B),
            phase,
            since_s: 1_800_000_000,
            active_slot: Slot::B,
            old,
            new: NewChild { open_streams: 3 },
            nodes_updated,
            failure: None,
        });
    }

    pub fn calls(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().calls.clone()
    }

    pub fn starts(&self) -> usize {
        self.0.lock().unwrap().starts.len()
    }
}

impl Supervisor for FakeSupervisor {
    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("start");
        if s.start_fails {
            return Err(SupervisorError::Failed("no candidate slot".to_owned()));
        }
        s.starts.push(spec.clone());
        s.status.update = Some(RunningUpdate {
            update_id: spec.update_id.clone(),
            from: spec.from.clone().unwrap_or_else(|| c(A)),
            to: spec.to.clone(),
            phase: Phase::Preflight,
            since_s: 1_800_000_000,
            active_slot: Slot::A,
            old: None,
            new: NewChild::default(),
            nodes_updated: 0,
            failure: None,
        });
        Ok(())
    }

    fn abort(&mut self) -> Result<Phase, SupervisorError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("abort");
        match s.status.update.as_mut().filter(|u| u.phase.is_open()) {
            Some(u) => {
                u.phase = Phase::RolledBack;
                Ok(Phase::RolledBack)
            }
            None => Err(SupervisorError::NotOpen),
        }
    }

    fn rollback(&mut self) -> Result<Phase, SupervisorError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("rollback");
        match s.status.update.as_mut().filter(|u| u.phase.is_open()) {
            Some(u) => {
                u.phase = Phase::RolledBack;
                Ok(Phase::RolledBack)
            }
            None => Err(SupervisorError::NotOpen),
        }
    }

    fn force_retire(&mut self, _reason: &str) -> Result<Retired, SupervisorError> {
        let mut s = self.0.lock().unwrap();
        s.calls.push("force_retire");
        let dropped = s
            .status
            .update
            .as_mut()
            .and_then(|u| u.old.take())
            .map_or(0, |o| o.open_streams);
        s.status.old_child_draining = false;
        Ok(Retired {
            dropped_streams: dropped,
        })
    }

    fn status(&self) -> SupervisorStatus {
        self.0.lock().unwrap().status.clone()
    }
}

#[derive(Clone)]
pub struct FakeGate {
    pub before: Arc<Mutex<Compat>>,
    pub staged: Arc<Mutex<Compat>>,
}

impl CompatGate for FakeGate {
    fn check(&self, _commit: &CommitId, facts: Option<&StagedFacts>) -> Compat {
        match facts {
            None => self.before.lock().unwrap().clone(),
            Some(_) => self.staged.lock().unwrap().clone(),
        }
    }
}

/// Dies at the named point, once.
pub struct DieAt(pub Arc<Mutex<Option<CrashPoint>>>);

impl CrashHook for DieAt {
    fn at(&mut self, point: CrashPoint) -> Result<(), Crash> {
        let mut armed = self.0.lock().unwrap();
        if *armed == Some(point) {
            *armed = None;
            Err(Crash)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Default)]
pub struct FakeForwarder {
    pub seen: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl Forwarder for FakeForwarder {
    async fn forward(&self, req: Request<Bytes>) -> Result<Response<Bytes>, ForwardError> {
        self.seen
            .lock()
            .unwrap()
            .push((req.method().to_string(), req.uri().path().to_owned()));
        if req.uri().path() == "/v1/update/pause" {
            return Err(ForwardError("connection refused".to_owned()));
        }
        Ok(Response::new(Bytes::from_static(b"{\"forwarded\":true}")))
    }
}

pub fn facts() -> StagedFacts {
    StagedFacts {
        protocol: 5,
        min_protocol: 3,
        state_schema: 7,
        profile: None,
        platform: None,
    }
}

pub fn idle() -> InstallerStatus {
    InstallerStatus {
        installed_commit: Some(c(A)),
        ..InstallerStatus::default()
    }
}

/// A status in which the installer has staged `commit`.
pub fn staged(commit: &str) -> InstallerStatus {
    InstallerStatus {
        installed_commit: Some(c(A)),
        staged_commit: Some(c(commit)),
        state: InstallerState::Staged,
        staged: Some(facts()),
        ..InstallerStatus::default()
    }
}

/// Every seam, with handles to look at and change them.
pub struct Rig {
    pub dir: PathBuf,
    pub status: FakeStatus,
    pub orders: FakeOrders,
    pub sup: FakeSupervisor,
    pub gate: FakeGate,
    pub clock: Arc<FakeClock>,
    pub die: Arc<Mutex<Option<CrashPoint>>>,
}

impl Rig {
    pub fn new(name: &str) -> Rig {
        let dir = kbf_host::testkit::scratch(name);
        Rig {
            dir,
            status: FakeStatus(Arc::new(Mutex::new(Ok(idle())))),
            orders: FakeOrders::default(),
            sup: FakeSupervisor::default(),
            gate: FakeGate {
                before: Arc::new(Mutex::new(Compat::Compatible)),
                staged: Arc::new(Mutex::new(Compat::Compatible)),
            },
            clock: Arc::new(FakeClock(AtomicU64::new(0))),
            die: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_status(&self, s: InstallerStatus) {
        *self.status.0.lock().unwrap() = Ok(s);
    }

    pub fn marker_path(&self) -> PathBuf {
        self.dir.join("restart-pending")
    }

    /// A core over this rig's state directory (a second call is a restarted process).
    pub fn core(&self) -> Core {
        Core::new(Parts {
            supervisor: Box::new(self.sup.clone()),
            orders: Box::new(self.orders.clone()),
            status: Box::new(self.status.clone()),
            compat: Box::new(self.gate.clone()),
            clock: self.clock.clone(),
            marker: RestartMarker::new(self.marker_path()),
            requests: Idempotency::open(&self.dir),
            hook: Box::new(DieAt(self.die.clone())),
            config: Config::default(),
        })
    }
}

pub fn req(commit: &str, request_id: &str) -> super::model::UpdateRequest {
    super::model::UpdateRequest {
        commit: c(commit),
        request_id: request_id.to_owned(),
        nodes: true,
        rollback: false,
    }
}

pub const COMMIT_C: &str = C;
