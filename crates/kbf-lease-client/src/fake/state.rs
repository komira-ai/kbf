//! The fake lease server's rules, with no I/O. Each route is a method that takes what the
//! route parsed and returns the status and JSON body the real server would answer with, or
//! its refusal.

use std::collections::BTreeMap;
use std::str::FromStr as _;

use serde::Serialize;

use crate::ids::{LeaseId, RunId};
use crate::wire::{
    AcquireRequest, Granted, Host, HostList, HostLoad, HostSource, HostState, Lease, LeaseList,
    LeaseState, Mount, Outcome, PendingLease, ReleaseState, Released, RenewRequest, Renewed, Run,
    RunAccepted, RunFile, RunList, RunSpec, RunState, Shape, Taint,
};

/// The ttl of a lease that asks for none (the server's `--default-ttl`).
const DEFAULT_TTL_MS: u64 = 600_000;
/// The longest ttl (the server's `--max-ttl`).
const MAX_TTL_MS: u64 = 3_600_000;
/// The longest wall time of a run (the server's seven days).
const MAX_WALL_MS: u64 = 7 * 24 * 3_600_000;
/// The most live runs on one host.
const MAX_LIVE_RUNS: usize = 256;

/// What the fake serves: one class, one shape, the hosts of the pool.
#[derive(Debug, Clone)]
pub struct Pool {
    /// The class a lease must ask for.
    pub class: String,
    /// What each host holds, and so what a lease holds and what its runs may add up to.
    pub shape: Shape,
    /// The capabilities every host reports.
    pub capabilities: BTreeMap<String, String>,
    /// The hosts, by id.
    pub hosts: Vec<String>,
    /// Whether a lease is `202 pending` until [`FakeLeaseServer::ready`] says its host is
    /// prepared, as a `whole-node` lease is.
    ///
    /// [`FakeLeaseServer::ready`]: super::FakeLeaseServer::ready
    pub prepare: bool,
}

impl Pool {
    /// The same pool with its hosts reporting `run-files=true`, as an agent with a usable
    /// files root does. The fake models it per pool; the real server takes the capability
    /// from each host's own hello. Without it a start with files is `409 files-unsupported`. Pools are
    /// built without it so that a host list has the capabilities it always had.
    #[must_use]
    pub fn with_run_files(mut self) -> Self {
        self.capabilities
            .insert("run-files".to_owned(), "true".to_owned());
        self
    }

    /// A pool of `hosts`, each of `cpu_millis` and `memory_bytes`, under `class`.
    #[must_use]
    pub fn new(class: &str, hosts: &[&str], cpu_millis: u64, memory_bytes: u64) -> Self {
        Self {
            class: class.to_owned(),
            shape: Shape {
                cpu_millis,
                memory_bytes,
                gpus: 0,
                gpu_class: None,
                disk_bytes: None,
            },
            capabilities: BTreeMap::from([("os".to_owned(), "linux".to_owned())]),
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            prepare: false,
        }
    }
}

/// An error answer: a status and `{"error": code}`, with a `reason` for a 400.
#[derive(Debug)]
pub(super) struct Refusal {
    pub(super) status: u16,
    code: Option<&'static str>,
    reason: Option<String>,
}

impl Refusal {
    fn new(status: u16, code: &'static str) -> Self {
        Self {
            status,
            code: Some(code),
            reason: None,
        }
    }

    /// The 400 of a body the route cannot read. It says what kind of fault and where, and
    /// never repeats the body.
    pub(super) fn bad_body(error: &serde_json::Error) -> Self {
        Self::bad_request(format!(
            "the body is {} (line {}, column {})",
            match error.classify() {
                serde_json::error::Category::Data => "JSON, but not what this route takes",
                _ => "not valid JSON",
            },
            error.line(),
            error.column()
        ))
    }

    fn bad_request(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::new(400, "bad-request")
        }
    }

    pub(super) fn unknown_lease() -> Self {
        Self::new(404, "unknown-lease")
    }

    pub(super) fn not_found() -> Self {
        Self::new(404, "not-found")
    }

    /// The 405 of a known route with another method: axum answers it with no body.
    pub(super) fn method_not_allowed() -> Self {
        Self {
            status: 405,
            code: None,
            reason: None,
        }
    }

    pub(super) fn body(&self) -> Vec<u8> {
        let Some(code) = self.code else {
            return Vec::new();
        };
        let mut body = serde_json::json!({ "error": code });
        if let Some(reason) = &self.reason {
            body["reason"] = serde_json::Value::String(reason.clone());
        }
        body.to_string().into_bytes()
    }
}

/// A status and a JSON body, or a refusal.
pub(super) type Answer = Result<(u16, Vec<u8>), Refusal>;

fn json<T: Serialize>(status: u16, body: &T) -> Answer {
    Ok((status, serde_json::to_vec(body).unwrap_or_default()))
}

struct LeaseRow {
    state: LeaseState,
    host: String,
    acquired_at_ms: u64,
    expires_at_ms: u64,
    ends_at_ms: u64,
    labels: BTreeMap<String, String>,
}

#[derive(Default)]
struct HostRow {
    lease: Option<LeaseId>,
    lost: bool,
    /// Known, and silent since the server restarted.
    awaiting: bool,
    taint: Option<Taint>,
    /// A code the host refuses every new run with, short of a taint.
    refusing: Option<&'static str>,
    load: Option<HostLoad>,
}

struct RunRow {
    id: RunId,
    lease: LeaseId,
    state: RunState,
    outcome: Option<Outcome>,
    exit_code: Option<i32>,
    detail: Option<String>,
    cpu_millis: u64,
    memory_bytes: u64,
    wall_ms: u64,
    disk_bytes: Option<u64>,
    args: Vec<String>,
    image: String,
    mounts: Vec<Mount>,
    /// Secret: kept for the test that plays the host, never in a body.
    files: Vec<RunFile>,
    labels: BTreeMap<String, String>,
}

impl RunRow {
    /// A run whose limits are still booked: it has not `ended` and was not given up.
    fn live(&self) -> bool {
        !matches!(self.state, RunState::Ended | RunState::NeverStarted)
    }

    fn view(&self) -> Run {
        Run {
            run_id: self.id.clone(),
            state: self.state,
            outcome: self.outcome,
            exit_code: self.exit_code,
            detail: self.detail.clone(),
            labels: self.labels.clone(),
        }
    }
}

pub(super) struct State {
    pool: Pool,
    term: u64,
    next_lease: u64,
    next_run: u64,
    now_ms: u64,
    leases: BTreeMap<LeaseId, LeaseRow>,
    hosts: BTreeMap<String, HostRow>,
    /// Oldest first, as the list shows them.
    runs: Vec<RunRow>,
}

/// The clock the fake starts at, in milliseconds.
const START_MS: u64 = 1_793_000_000_000;

impl State {
    pub(super) fn new(pool: Pool) -> Self {
        let hosts = pool
            .hosts
            .iter()
            .map(|host| (host.clone(), HostRow::default()))
            .collect();
        Self {
            pool,
            term: 4,
            next_lease: 1,
            next_run: 1,
            now_ms: START_MS,
            leases: BTreeMap::new(),
            hosts,
            runs: Vec::new(),
        }
    }

    // ---- the routes

    pub(super) fn acquire(&mut self, request: AcquireRequest) -> Answer {
        if request.class != self.pool.class {
            return Err(Refusal::new(422, "unknown-class"));
        }
        if request.max_wall_seconds == 0 {
            return Err(Refusal::new(422, "wall-time-not-positive"));
        }
        if request.labels.keys().any(String::is_empty) {
            return Err(Refusal::new(422, "invalid-label"));
        }
        if request.ttl_seconds == Some(0) {
            return Err(Refusal::bad_request("ttl_seconds must be positive"));
        }
        let shape = &self.pool.shape;
        let resources = &request.resources;
        if resources.cpu_millis > shape.cpu_millis
            || resources.memory_bytes > shape.memory_bytes
            || resources.gpus > shape.gpus
        {
            return Err(Refusal::new(409, "no-fit"));
        }
        let wanted = &request.capabilities;
        let capable = wanted
            .iter()
            .all(|(key, value)| self.pool.capabilities.get(key) == Some(value));
        if !capable {
            return Err(Refusal::new(409, "no-capable-host"));
        }
        let free = self
            .hosts
            .iter()
            .find(|(_, host)| host.lease.is_none() && !host.lost && !host.awaiting)
            .map(|(id, _)| id.clone());
        let Some(host) = free else {
            return Err(Refusal::new(409, "no-capacity"));
        };
        let id = LeaseId::new(self.term, self.next_lease);
        self.next_lease += 1;
        let ends_at_ms = self.now_ms + request.max_wall_seconds.saturating_mul(1000);
        let ttl_ms = request
            .ttl_seconds
            .map_or(DEFAULT_TTL_MS, |seconds| seconds.saturating_mul(1000))
            .min(MAX_TTL_MS);
        let expires_at_ms = (self.now_ms + ttl_ms).min(ends_at_ms);
        let pending = self.pool.prepare;
        self.leases.insert(
            id,
            LeaseRow {
                state: if pending {
                    LeaseState::Pending
                } else {
                    LeaseState::Held
                },
                host: host.clone(),
                acquired_at_ms: self.now_ms,
                expires_at_ms,
                ends_at_ms,
                labels: request.labels,
            },
        );
        self.hosts.entry(host.clone()).or_default().lease = Some(id);
        match pending {
            true => json(
                202,
                &PendingLease {
                    id,
                    state: LeaseState::Pending,
                },
            ),
            false => json(
                201,
                &Granted {
                    id,
                    host,
                    expires_at_ms,
                },
            ),
        }
    }

    pub(super) fn renew(&mut self, id: LeaseId, request: &RenewRequest) -> Answer {
        if request.ttl_seconds == Some(0) {
            return Err(Refusal::bad_request("ttl_seconds must be positive"));
        }
        let now = self.now_ms;
        let row = self
            .leases
            .get_mut(&id)
            .ok_or_else(Refusal::unknown_lease)?;
        if row.state != LeaseState::Held {
            return Err(Refusal::new(409, "not-held"));
        }
        let ttl_ms = request
            .ttl_seconds
            .map_or(DEFAULT_TTL_MS, |seconds| seconds.saturating_mul(1000))
            .min(MAX_TTL_MS);
        row.expires_at_ms = (now + ttl_ms).min(row.ends_at_ms);
        json(
            200,
            &Renewed {
                expires_at_ms: row.expires_at_ms,
                host_reserved: false,
            },
        )
    }

    pub(super) fn release(&mut self, id: LeaseId) -> Answer {
        if !self.leases.contains_key(&id) {
            return Err(Refusal::unknown_lease());
        }
        match self.drop_lease(id) {
            ReleaseState::Releasing => json(
                202,
                &Released {
                    state: ReleaseState::Releasing,
                },
            ),
            _ => json(
                200,
                &Released {
                    state: ReleaseState::Freed,
                },
            ),
        }
    }

    pub(super) fn leases(&self) -> Answer {
        let leases = self
            .leases
            .iter()
            .map(|(id, row)| Lease {
                id: *id,
                state: row.state,
                host: Some(row.host.clone()),
                acquired_at_ms: row.acquired_at_ms,
                expires_at_ms: row.expires_at_ms,
                ends_at_ms: row.ends_at_ms,
                labels: row.labels.clone(),
            })
            .collect();
        json(200, &LeaseList { leases })
    }

    pub(super) fn hosts(&self) -> Answer {
        let hosts = self
            .hosts
            .iter()
            .map(|(id, row)| Host {
                id: id.clone(),
                source: HostSource::Registered,
                shape: self.pool.shape.clone(),
                capabilities: self.pool.capabilities.clone(),
                labels: BTreeMap::new(),
                lease: row.lease,
                state: Some(match (row.lost, row.awaiting) {
                    (true, _) => HostState::Lost,
                    (false, true) => HostState::Awaiting,
                    (false, false) => HostState::Live,
                }),
                lost: Some(row.lost),
                phase: Some("serving".to_owned()),
                agent_version: Some("fake".to_owned()),
                releasing: Some(false),
                taint: row.taint.clone(),
                mark: None,
                reserved_for: None,
                drain_deadline_ms: None,
                // A lost host lists none, as the server does.
                load: row.load.clone().filter(|_| !row.lost),
            })
            .collect();
        json(200, &HostList { hosts })
    }

    pub(super) fn queue_run(&mut self, id: LeaseId, spec: RunSpec) -> Answer {
        // A bad file is the body's fault, whatever the lease or the host: the server answers
        // it before it reads any state, and the answer repeats no name or content.
        if !files_valid(&spec.files) {
            return Err(Refusal::new(400, "invalid-file"));
        }
        let lease = self.leases.get(&id).ok_or_else(Refusal::unknown_lease)?;
        let left = lease
            .expires_at_ms
            .min(lease.ends_at_ms)
            .saturating_sub(self.now_ms);
        if lease.state != LeaseState::Held {
            return Err(Refusal::new(409, "not-held"));
        }
        // The server's order: the lease, the host's record, the job's vetting, and only then
        // what the host is doing.
        let host = &self.hosts[&lease.host];
        if host.awaiting {
            return Err(Refusal::new(409, "host-not-registered"));
        }
        let limits = &spec.limits;
        if limits.cpu_millis == 0 || limits.memory_bytes == 0 || limits.wall_ms == Some(0) {
            return Err(Refusal::new(422, "invalid-job"));
        }
        if !pinned(&spec.image) {
            return Err(Refusal::new(422, "image-digest-required"));
        }
        if spec.isolation == crate::wire::Isolation::Host {
            return Err(Refusal::new(422, "host-jobs-not-allowed"));
        }
        let shape = &self.pool.shape;
        let gpu_beyond = spec
            .gpu_indices
            .iter()
            .any(|index| u64::from(*index) >= shape.gpus);
        let disk_beyond = limits
            .disk_bytes
            .zip(shape.disk_bytes)
            .is_some_and(|(asked, held)| asked > held);
        if limits.cpu_millis > shape.cpu_millis
            || limits.memory_bytes > shape.memory_bytes
            || disk_beyond
            || gpu_beyond
        {
            return Err(Refusal::new(422, "limits-exceed-shape"));
        }
        // After the vetting, as the registry has it, and before the host's state: a host that
        // cannot deliver the files is refused for that, lost or tainted or not.
        let delivers = self.pool.capabilities.get("run-files").map(String::as_str) == Some("true");
        if !spec.files.is_empty() && !delivers {
            return Err(Refusal::new(409, "files-unsupported"));
        }
        if host.lost {
            return Err(Refusal::new(409, "host-lost"));
        }
        if host.taint.is_some() {
            return Err(Refusal::new(409, "host-tainted"));
        }
        if let Some(code) = host.refusing {
            return Err(Refusal::new(409, code));
        }
        let live = self.runs.iter().filter(|run| run.lease == id && run.live());
        let (count, cpu, memory) = live.fold((0, 0, 0), |(count, cpu, memory), run| {
            (count + 1, cpu + run.cpu_millis, memory + run.memory_bytes)
        });
        if count >= MAX_LIVE_RUNS
            || cpu + limits.cpu_millis > shape.cpu_millis
            || memory + limits.memory_bytes > shape.memory_bytes
        {
            return Err(Refusal::new(409, "no-capacity"));
        }
        let run_id = RunId::minted(self.term, self.next_run);
        self.next_run += 1;
        let cap = left.min(MAX_WALL_MS);
        let row = RunRow {
            id: run_id,
            lease: id,
            state: RunState::Queued,
            outcome: None,
            exit_code: None,
            detail: None,
            cpu_millis: limits.cpu_millis,
            memory_bytes: limits.memory_bytes,
            wall_ms: limits.wall_ms.map_or(cap, |asked| asked.min(cap)),
            disk_bytes: limits.disk_bytes,
            args: spec.args.clone(),
            image: spec.image.clone(),
            mounts: spec.mounts.clone(),
            files: spec.files.clone(),
            labels: spec.labels.clone(),
        };
        let accepted = RunAccepted {
            run_id: row.id.clone(),
            state: row.state,
        };
        self.runs.push(row);
        json(202, &accepted)
    }

    pub(super) fn run(&self, id: LeaseId, run_id: &str) -> Answer {
        self.require_lease(id)?;
        self.remembered(run_id)?;
        let run = self.find_run(id, run_id)?;
        json(200, &run.view())
    }

    pub(super) fn runs(&self, id: LeaseId) -> Answer {
        self.require_lease(id)?;
        let runs = self
            .runs
            .iter()
            .filter(|run| run.lease == id)
            .map(RunRow::view)
            .collect();
        json(200, &RunList { runs })
    }

    pub(super) fn stop_run(&mut self, id: LeaseId, run_id: &str) -> Answer {
        self.require_lease(id)?;
        self.remembered(run_id)?;
        let at = self.find_run(id, run_id)?.id.clone();
        let run = self.runs.iter_mut().find(|run| run.id == at);
        let state = run.map_or(RunState::Unknown, |run| {
            run.state = match run.state {
                RunState::Queued => RunState::NeverStarted,
                RunState::Starting | RunState::Running => RunState::Stopping,
                other => other,
            };
            run.state
        });
        json(202, &RunAccepted { run_id: at, state })
    }

    // ---- what the world does

    /// Expires the leases whose time is up, as the server's tick does.
    pub(super) fn advance(&mut self, millis: u64) {
        self.now_ms += millis;
        let due: Vec<LeaseId> = self
            .leases
            .iter()
            .filter(|(_, row)| row.expires_at_ms.min(row.ends_at_ms) <= self.now_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.drop_lease(id);
        }
    }

    pub(super) fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// A restart: the term goes up, every run is forgotten (the hosts are told to cancel
    /// them), leases persist, and every host is silent until it says hello again.
    pub(super) fn restart(&mut self) {
        self.term += 1;
        self.next_run = 1;
        self.runs.clear();
        for host in self.hosts.values_mut() {
            host.awaiting = true;
        }
        let releasing: Vec<LeaseId> = self
            .leases
            .iter()
            .filter(|(_, row)| row.state == LeaseState::Releasing)
            .map(|(id, _)| *id)
            .collect();
        for id in releasing {
            self.remove_lease(id);
        }
    }

    pub(super) fn host_hello(&mut self, host: &str) -> bool {
        self.hosts
            .get_mut(host)
            .map(|row| row.awaiting = false)
            .is_some()
    }

    pub(super) fn ready(&mut self, id: LeaseId) -> bool {
        let pending = self
            .leases
            .get_mut(&id)
            .filter(|row| row.state == LeaseState::Pending);
        pending.map(|row| row.state = LeaseState::Held).is_some()
    }

    pub(super) fn set_lost(&mut self, host: &str, lost: bool) -> bool {
        self.hosts
            .get_mut(host)
            .map(|row| row.lost = lost)
            .is_some()
    }

    pub(super) fn refuse_posts(&mut self, host: &str, code: Option<&'static str>) -> bool {
        self.hosts
            .get_mut(host)
            .map(|row| row.refusing = code)
            .is_some()
    }

    pub(super) fn set_load(&mut self, host: &str, load: Option<HostLoad>) -> bool {
        self.hosts
            .get_mut(host)
            .map(|row| row.load = load)
            .is_some()
    }

    pub(super) fn taint(&mut self, host: &str, reason: &str) -> bool {
        let taint = Taint {
            reason: reason.to_owned(),
            run_id: None,
            check: None,
        };
        self.hosts
            .get_mut(host)
            .map(|row| row.taint = Some(taint))
            .is_some()
    }

    pub(super) fn run_started(&mut self, run: &RunId) -> bool {
        let queued = self.runs.iter_mut().find(|row| {
            row.id == *run && matches!(row.state, RunState::Queued | RunState::Starting)
        });
        queued.map(|row| row.state = RunState::Running).is_some()
    }

    /// The host reports a result: the run is `exited` and keeps its limits booked.
    pub(super) fn finish_run(
        &mut self,
        run: &RunId,
        outcome: Outcome,
        exit_code: Option<i32>,
        detail: Option<&str>,
    ) -> bool {
        let live = self
            .runs
            .iter_mut()
            .find(|row| row.id == *run && row.live());
        live.map(|row| {
            row.state = RunState::Exited;
            row.outcome = Some(outcome);
            row.exit_code = exit_code;
            row.detail = detail.map(str::to_owned);
        })
        .is_some()
    }

    /// The host stops listing the run: it is `ended` and its limits come back.
    pub(super) fn end_run(&mut self, run: &RunId) -> bool {
        let exited = self
            .runs
            .iter_mut()
            .find(|row| row.id == *run && row.state == RunState::Exited);
        let lease = exited.map(|row| {
            row.state = RunState::Ended;
            row.lease
        });
        lease.map(|lease| self.settle(lease)).is_some()
    }

    pub(super) fn run_disk_bytes(&self, run: &RunId) -> Option<u64> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .and_then(|row| row.disk_bytes)
    }

    pub(super) fn run_wall_ms(&self, run: &RunId) -> Option<u64> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .map(|row| row.wall_ms)
    }

    pub(super) fn run_args(&self, run: &RunId) -> Option<Vec<String>> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .map(|row| row.args.clone())
    }

    pub(super) fn run_image(&self, run: &RunId) -> Option<String> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .map(|row| row.image.clone())
    }

    pub(super) fn run_mounts(&self, run: &RunId) -> Option<Vec<Mount>> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .map(|row| row.mounts.clone())
    }

    pub(super) fn run_files(&self, run: &RunId) -> Option<Vec<RunFile>> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .map(|row| row.files.clone())
    }

    pub(super) fn run_state(&self, run: &RunId) -> Option<RunState> {
        self.runs
            .iter()
            .find(|row| row.id == *run)
            .map(|row| row.state)
    }

    // ---- helpers

    fn require_lease(&self, id: LeaseId) -> Result<(), Refusal> {
        match self.leases.contains_key(&id) {
            true => Ok(()),
            false => Err(Refusal::unknown_lease()),
        }
    }

    /// 410 for the id of an earlier boot: the restart took the run.
    fn remembered(&self, run_id: &str) -> Result<(), Refusal> {
        let term = run_id
            .strip_prefix("run-")
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(term, _)| term.parse::<u64>().ok());
        match term {
            Some(term) if term < self.term => Err(Refusal::new(410, "run-forgotten")),
            _ => Ok(()),
        }
    }

    fn find_run(&self, id: LeaseId, run_id: &str) -> Result<&RunRow, Refusal> {
        let found = RunId::from_str(run_id).ok().and_then(|run_id| {
            self.runs
                .iter()
                .find(|run| run.lease == id && run.id == run_id)
        });
        found.ok_or_else(|| Refusal::new(404, "unknown-run"))
    }

    /// Ends or expires a lease: its live runs are asked to stop, and the lease is gone when
    /// none is left, `releasing` until then.
    fn drop_lease(&mut self, id: LeaseId) -> ReleaseState {
        for run in self.runs.iter_mut().filter(|run| run.lease == id) {
            run.state = match run.state {
                RunState::Queued => RunState::NeverStarted,
                RunState::Starting | RunState::Running => RunState::Stopping,
                other => other,
            };
        }
        self.leases
            .entry(id)
            .and_modify(|row| row.state = LeaseState::Releasing);
        self.settle(id)
    }

    /// Removes a releasing lease whose runs have all ended.
    fn settle(&mut self, id: LeaseId) -> ReleaseState {
        let releasing = self
            .leases
            .get(&id)
            .is_some_and(|row| row.state == LeaseState::Releasing);
        let busy = self.runs.iter().any(|run| run.lease == id && run.live());
        match releasing && !busy {
            true => {
                self.remove_lease(id);
                ReleaseState::Freed
            }
            false => ReleaseState::Releasing,
        }
    }

    fn remove_lease(&mut self, id: LeaseId) {
        self.runs.retain(|run| run.lease != id);
        let removed = self.leases.remove(&id);
        removed
            .into_iter()
            .for_each(|row| self.hosts.entry(row.host).or_default().lease = None);
    }
}

/// The lease server's rules for the files of a start: at most 8; a name of `[A-Za-z0-9._-]`,
/// leading alphanumeric, 1 to 64 bytes, none twice ignoring case; 16 KiB of contents a file
/// and 64 KiB in all; no NUL.
fn files_valid(files: &[RunFile]) -> bool {
    const MAX_FILES: usize = 8;
    const MAX_NAME: usize = 64;
    const MAX_FILE: usize = 16 * 1024;
    const MAX_TOTAL: usize = 64 * 1024;
    let name_ok = |name: &str| {
        let leading = name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
        let grammar = name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        leading && grammar && name.len() <= MAX_NAME
    };
    let mut seen = std::collections::BTreeSet::new();
    let each = files.iter().all(|file| {
        name_ok(&file.name)
            && file.contents.len() <= MAX_FILE
            && !file.contents.contains('\0')
            && seen.insert(file.name.to_ascii_lowercase())
    });
    let total: usize = files.iter().map(|file| file.contents.len()).sum();
    files.len() <= MAX_FILES && each && total <= MAX_TOTAL
}

/// Whether an image reference is pinned by digest, `name@sha256:<64 hex>` (a tag before the
/// `@` is fine).
fn pinned(image: &str) -> bool {
    image.rsplit_once("@sha256:").is_some_and(|(name, hex)| {
        !name.is_empty() && hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}
