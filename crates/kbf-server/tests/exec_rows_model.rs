//! The protocol of `docs/design/n-frontends.md`, section 8, searched exhaustively over the
//! real row API of `kbf_server::executor::rows`.
//!
//! One worker, one lease, N frontends. Every interleaving of these steps is walked
//! breadth-first: the row insert before the submit, the submit, the job ending by itself,
//! the ingest, the report, the row delete, a beat, the expiry of a liveness row on the
//! store's clock, a claim, the registration with the farm that follows a claim, a release,
//! a retirement, a self-fence, a crash and a restart. Each step that touches a row calls
//! the row API against an in-memory store; the job service and the farm are modelled here
//! (the job service: submitted, ended; the farm: one accepted result, from the frontend
//! that registered last), and so is the store's clock (a liveness row expires when the
//! search says so; no real time passes).
//!
//! What is checked, in every reachable state:
//!
//! - no frontend wrote the job row without holding the worker row (the version it holds is
//!   the worker row's current version);
//! - when a liveness row expires only after its holder stopped acting (the fence of section
//!   4.3.5 holds), at most one frontend believes it holds the worker;
//! - from every state a result is reachable: success, an infrastructure failure, or a
//!   requeue (no stuck lease). States where the claim count has reached its cap are not
//!   judged, since the cap ends the search there.
//!
//! The search is run with the guards of the transactions in place, and with each of two
//! guards taken out of the transaction the API builds: the claim's check that the owner's
//! liveness row is absent, and the job writes' check of the worker row. Each of those runs
//! must find a violation, so the search can be seen to fail when a guard is missing.
//!
//! The states of the search are the states of the rows, not of the Python specification of
//! the design: a holder that lost the worker is one state however many claims ago it did.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_server::executor::rows::{
    ExecRows, Held, JobRow, Live, Liveness, RowsError, Shape, WorkerRow, frontend_key, worker_key,
};
use kbf_state::{
    Change, Changes, Commit, CommitStats, Cond, Conflict, Op, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Version, Versioned,
};

/// The most claims of the worker one search allows, the first row included.
const MAX_GENERATION: u8 = 4;
const TTL: Duration = Duration::from_secs(45);
const WORKER: &str = "w";
const JOB: &str = "job";

// ---- the store ----------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Data {
    rows: std::collections::BTreeMap<String, (Vec<u8>, Version)>,
    revision: u64,
}

/// A store that keeps its rows in a map and can be forked, which the search needs to try a
/// step on a copy. It applies a transaction as the contract says: every precondition is
/// judged against the rows before the transaction, and a failed one writes nothing. Reads
/// of the change feed and the clock are not part of what the rows use.
struct Mem(Mutex<Data>);

impl Mem {
    fn new() -> Arc<Mem> {
        Arc::new(Mem(Mutex::default()))
    }

    fn fork(&self) -> Arc<Mem> {
        Arc::new(Mem(Mutex::new(self.data().clone())))
    }

    fn data(&self) -> std::sync::MutexGuard<'_, Data> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn version(&self, key: &str) -> Option<Version> {
        self.data().rows.get(key).map(|(_, version)| *version)
    }

    fn value(&self, key: &str) -> Option<Vec<u8>> {
        self.data().rows.get(key).map(|(value, _)| value.clone())
    }

    /// The store's clock reaching the end of a key's time to live.
    fn expire(&self, key: &str) {
        self.data().rows.remove(key);
    }
}

fn versioned(row: &(Vec<u8>, Version)) -> Versioned {
    Versioned {
        value: row.0.clone(),
        version: row.1,
        expires_at: None,
    }
}

impl Store for Mem {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let mut data = self.data();
        let mut results = Vec::new();
        for txn in batch {
            let failed = txn.ops().iter().enumerate().find_map(|(index, op)| {
                let (key, cond) = op.key_cond()?;
                let held = data.rows.get(key).map(|(_, version)| *version);
                let met = match cond {
                    Cond::Any => true,
                    Cond::Absent => held.is_none(),
                    Cond::Version(version) => held == Some(version),
                };
                (!met).then(|| Conflict {
                    index,
                    key: key.to_owned(),
                })
            });
            if let Some(conflict) = failed {
                results.push(Err(conflict));
                continue;
            }
            for op in txn.ops() {
                match op {
                    Op::Put { key, value, .. } => {
                        data.revision += 1;
                        let version = Version(data.revision);
                        data.rows.insert(key.clone(), (value.clone(), version));
                    }
                    Op::Delete { key, .. } => {
                        data.revision += 1;
                        data.rows.remove(key);
                    }
                    Op::Check { .. } | Op::Next { .. } => {}
                }
            }
            results.push(Ok(Commit {
                revision: Revision(data.revision),
                counters: Vec::new(),
            }));
        }
        Ok(results)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        Ok(self.data().rows.get(key).map(versioned))
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let data = self.data();
        let rows = data.rows.range(prefix.to_owned()..);
        Ok(rows
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, row)| (key.clone(), versioned(row)))
            .collect())
    }

    fn scan_range(&self, _: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        Err(StoreError::Failed("not used by the rows".to_owned()))
    }

    fn changes(&self, _: Revision, _: usize) -> Result<Changes, StoreError> {
        Ok(Changes {
            rows: Vec::<Change>::new(),
            cursor: Revision::ZERO,
        })
    }

    fn trim_feed(&self, _: Revision) -> Result<u64, StoreError> {
        Ok(0)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        Ok(0)
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats::default()
    }

    fn now(&self) -> StoreTime {
        StoreTime::ZERO
    }
}

// ---- the guards under test ---------------------------------------------------------------

/// Which guard the search takes out of the transactions the API builds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guards {
    /// None: the transactions as the API builds them.
    All,
    /// The claim without its check that the owner's liveness row is absent.
    NoOwnerCheck,
    /// The job-row writes without their check of the worker row.
    NoWorkerCheck,
}

/// `txn` without its checks of `prefix`.
fn without_checks(txn: &Txn, prefix: &str, cond: fn(Cond) -> bool) -> Txn {
    let mut out = Txn::new();
    for op in txn.ops() {
        out = match op.clone() {
            Op::Check { key, cond: c } if key.starts_with(prefix) && cond(c) => out,
            Op::Check { key, cond } => out.check(key, cond),
            Op::Put {
                key,
                value,
                ttl: Some(ttl),
                cond,
            } => out.put_ttl(key, value, ttl, cond),
            Op::Put {
                key, value, cond, ..
            } => out.put(key, value, cond),
            Op::Delete { key, cond } => out.delete(key, cond),
            Op::Next { counter } => out.next(counter),
        };
    }
    out
}

fn is_absent(cond: Cond) -> bool {
    cond == Cond::Absent
}

fn is_version(cond: Cond) -> bool {
    matches!(cond, Cond::Version(_))
}

// ---- the world ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Task {
    None,
    /// Claimed the worker; not yet registered with the farm and rebuilt from its rows.
    Adopting,
    /// The row is written; the job is not yet submitted.
    Prep,
    Run,
    Ingested,
    Reported,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Service {
    None,
    Running,
    Ended,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    None,
    Done,
    Requeued,
    Infra,
}

#[derive(Clone)]
struct Frontend {
    up: bool,
    held: Option<Held>,
    live: Option<Live>,
    task: Task,
    /// The farm's owner counter when this frontend last registered the node.
    farm: u8,
}

#[derive(Clone)]
struct World {
    store: Arc<Mem>,
    fes: Vec<Frontend>,
    service: Service,
    /// The farm's owner counter of the node.
    farm: u8,
    result: Outcome,
    ingests: u8,
    /// Set when a frontend wrote a job row without holding the worker.
    bad: bool,
    claims: u8,
    /// The job row was written and deleted again after its result.
    done: bool,
}

fn instance(i: usize) -> String {
    format!("i{i}")
}

fn alive() -> Liveness {
    Liveness {
        group: "kbf".to_owned(),
        started_ms: 0,
        release: "model".to_owned(),
        draining: false,
    }
}

fn worker_row(owner: &str) -> WorkerRow {
    WorkerRow {
        owner_instance: owner.to_owned(),
        farm_instance: format!("farm-{owner}"),
        shape: Shape {
            cpu_millis: 1_000,
            memory_bytes: 1 << 30,
            slots: 1,
            arch: "x86_64".to_owned(),
        },
    }
}

fn job_row() -> JobRow {
    JobRow {
        lease_term: 1,
        lease_seq: 1,
        action: "sha256:00/0".to_owned(),
        pack_key: "cas/b/model/nonce-0".to_owned(),
        submitted_ms: 0,
    }
}

impl World {
    /// The start of every search: frontend 0 holds the worker and every frontend is alive.
    fn start(n: usize) -> World {
        let store = Mem::new();
        let rows = ExecRows::new(store.clone() as Arc<dyn Store>);
        let mut fes = Vec::new();
        for i in 0..n {
            let live = rows.beat(&instance(i), &alive(), TTL, &[]).expect("beat");
            let held = (i == 0).then(|| {
                rows.create_worker(WORKER, &worker_row(&instance(0)))
                    .expect("create")
            });
            fes.push(Frontend {
                up: true,
                held,
                live: Some(live),
                task: Task::None,
                farm: u8::from(i == 0),
            });
        }
        World {
            store,
            fes,
            service: Service::None,
            farm: 1,
            result: Outcome::None,
            ingests: 0,
            bad: false,
            claims: 1,
            done: false,
        }
    }

    fn rows(&self, store: &Arc<Mem>) -> ExecRows {
        ExecRows::new(Arc::clone(store) as Arc<dyn Store>)
    }

    fn current(&self) -> Option<Version> {
        self.store.version(&worker_key(WORKER).expect("key"))
    }

    /// Whether the version frontend `i` holds is the worker row's now: whether a check of
    /// the row by it passes.
    fn holds_row(&self, i: usize) -> bool {
        let held = self.fes[i].held.as_ref();
        held.is_some_and(|h| Some(h.version) == self.current())
    }

    fn job_row_present(&self) -> bool {
        let key = format!("exec/j/{WORKER}/{JOB}");
        self.store.value(&key).is_some()
    }

    /// The world after frontend `i` found it no longer holds the worker: it drops what it
    /// holds and nothing is written.
    fn lost(&self, i: usize) -> World {
        let mut next = self.clone();
        next.fes[i].held = None;
        next.fes[i].task = Task::None;
        next
    }

    /// What the search distinguishes: the rows, and what each frontend and the modelled
    /// services hold, with a held version reduced to whether it is the worker row's now.
    fn key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        for (i, fe) in self.fes.iter().enumerate() {
            let held = match (&fe.held, self.holds_row(i)) {
                (None, _) => 0,
                (Some(_), true) => 1,
                (Some(_), false) => 2,
            };
            let live = self
                .store
                .value(&frontend_key(&instance(i)).expect("key"))
                .is_some();
            key.extend([
                u8::from(fe.up),
                held,
                fe.task as u8,
                fe.farm,
                u8::from(live),
            ]);
        }
        let worker = self.store.value(&worker_key(WORKER).expect("key"));
        key.push(match worker {
            None => 255,
            Some(bytes) => {
                let text = String::from_utf8_lossy(&bytes[1..]).into_owned();
                (0..self.fes.len())
                    .find(|i| text.contains(&format!("\"owner_instance\":\"{}\"", instance(*i))))
                    .map_or(254, |i| u8::try_from(i).expect("small"))
            }
        });
        key.extend([
            u8::from(self.job_row_present()),
            u8::from(self.done),
            self.service as u8,
            self.farm,
            self.result as u8,
            self.ingests,
            u8::from(self.bad),
            self.claims,
        ]);
        key
    }

    fn violation(&self, fenced: bool) -> Option<&'static str> {
        if fenced && self.fes.iter().filter(|f| f.up && f.held.is_some()).count() > 1 {
            return Some("two frontends act on the worker");
        }
        self.bad
            .then_some("a frontend wrote the job row without holding the worker row")
    }
}

struct Search {
    n: usize,
    /// Expiry of a liveness row only after its holder stopped acting.
    fenced: bool,
    guards: Guards,
}

impl Search {
    /// Runs `step` on a fork of the world's store: the fork if it committed, `None` if a
    /// precondition failed (the world's own store is then unchanged).
    fn on_fork<T>(
        &self,
        world: &World,
        step: impl FnOnce(&ExecRows) -> Result<T, RowsError>,
    ) -> (Arc<Mem>, Result<T, RowsError>) {
        let fork = world.store.fork();
        let result = step(&world.rows(&fork));
        (fork, result)
    }

    /// Every step any frontend or the environment can take in `w`, as (name, next world).
    fn successors(&self, w: &World) -> Vec<(String, World)> {
        let mut out: Vec<(String, World)> = Vec::new();
        for i in 0..self.n {
            self.frontend_steps(w, i, &mut out);
        }
        if w.service == Service::Running {
            let mut next = w.clone();
            next.service = Service::Ended;
            out.push(("job_ends".to_owned(), next));
        }
        out
    }

    fn frontend_steps(&self, w: &World, i: usize, out: &mut Vec<(String, World)>) {
        let name = |step: &str| format!("{step}({i})");
        let fe = &w.fes[i];
        let me = instance(i);
        if !fe.up {
            let (store, live) = self.on_fork(w, |rows| rows.beat(&me, &alive(), TTL, &[]));
            let mut next = w.clone();
            next.store = store;
            next.fes[i] = Frontend {
                up: true,
                held: None,
                live: Some(live.expect("beat")),
                task: Task::None,
                farm: 0,
            };
            out.push((name("restart"), next));
            return;
        }
        let holds = w.holds_row(i);

        // The row insert before the submit.
        if let Some(held) = &fe.held
            && fe.task == Task::None
            && !w.job_row_present()
            && !w.done
            && w.result == Outcome::None
        {
            let (store, wrote) = self.on_fork(w, |rows| match self.guards {
                Guards::NoWorkerCheck => {
                    let txn = ExecRows::insert_job_txn(held, JOB, &job_row())?;
                    rows.commit(without_checks(&txn, "exec/w/", is_version))
                        .map(drop)
                }
                _ => rows.insert_job(held, JOB, &job_row()).map(drop),
            });
            match wrote {
                Ok(()) => {
                    let mut next = w.clone();
                    next.store = store;
                    next.bad |= !holds;
                    next.fes[i].task = Task::Prep;
                    out.push((name("insert_job"), next));
                }
                Err(_) => out.push((name("lost"), w.lost(i))),
            }
        }
        // The submit to the job service (idempotent by job id).
        if fe.task == Task::Prep {
            let mut next = w.clone();
            if next.service == Service::None {
                next.service = Service::Running;
            }
            next.fes[i].task = Task::Run;
            out.push((name("submit"), next));
        }
        // The ingest of a job that ended: repeatable.
        if fe.task == Task::Run && w.service == Service::Ended {
            let mut next = w.clone();
            next.ingests = (next.ingests + 1).min(2);
            next.fes[i].task = Task::Ingested;
            out.push((name("ingest"), next));
        }
        // The report: the farm takes one result, from the frontend that registered last.
        if fe.task == Task::Ingested {
            let mut next = w.clone();
            if fe.farm == w.farm && w.result == Outcome::None {
                next.result = Outcome::Done;
                next.fes[i].task = Task::Reported;
                out.push((name("report"), next));
            } else {
                next.fes[i].task = Task::None;
                out.push((name("report_refused"), next));
            }
        }
        // The row delete after the farm's acknowledgement.
        if let (Task::Reported, Some(held)) = (fe.task, &fe.held) {
            match self.delete(w, held) {
                Ok(store) => {
                    let mut next = w.clone();
                    next.store = store;
                    next.bad |= !holds;
                    next.done = true;
                    next.fes[i].task = Task::None;
                    out.push((name("finish"), next));
                }
                Err(()) => out.push((name("lost"), w.lost(i))),
            }
        }
        // A beat of the liveness row; a holder's beat checks the worker row. A holder may
        // also give the worker up by itself (the self-fence).
        match &fe.held {
            Some(held) => {
                let (store, beat) = self.on_fork(w, |rows| {
                    rows.beat(&me, &alive(), TTL, std::slice::from_ref(held))
                });
                match beat {
                    Ok(live) => {
                        let mut next = w.clone();
                        next.store = store;
                        next.fes[i].live = Some(live);
                        out.push((name("beat"), next));
                    }
                    Err(_) => out.push((name("lost"), w.lost(i))),
                }
                out.push((name("self_fence"), w.lost(i)));
            }
            None => {
                let (store, live) = self.on_fork(w, |rows| rows.beat(&me, &alive(), TTL, &[]));
                let mut next = w.clone();
                next.store = store;
                next.fes[i].live = Some(live.expect("beat"));
                out.push((name("beat"), next));
            }
        }
        // A hard stop hands the worker back, cancelling nothing.
        if let (Some(held), true) = (&fe.held, holds) {
            let (store, released) = self.on_fork(w, |rows| rows.release(held, &worker_row(&me)));
            if released.is_ok() {
                let mut next = w.clone();
                next.store = store;
                next.fes[i].held = None;
                next.fes[i].task = Task::None;
                out.push((name("release"), next));
            }
        }
        // A crash: the liveness row stays until it expires.
        {
            let mut next = w.clone();
            next.fes[i] = Frontend {
                up: false,
                held: None,
                live: None,
                task: Task::None,
                farm: 0,
            };
            out.push((name("crash"), next));
        }
        self.claim_steps(w, i, out);
        // The store's clock expires the liveness row.
        let key = frontend_key(&me).expect("key");
        if w.store.value(&key).is_some() && (!self.fenced || fe.held.is_none()) {
            let store = w.store.fork();
            store.expire(&key);
            let mut next = w.clone();
            next.store = store;
            out.push((name("expire"), next));
        }
        // The retirement of a worker whose job is done.
        if let (Some(held), true, Task::None, true) = (&fe.held, holds, fe.task, w.done) {
            let (store, retired) = self.on_fork(w, |rows| rows.retire(held));
            if retired.is_ok() {
                let mut next = w.clone();
                next.store = store;
                next.fes[i].held = None;
                out.push((name("retire"), next));
            }
        }
    }

    /// The delete of the job row by the holder `held`.
    fn delete(&self, w: &World, held: &Held) -> Result<Arc<Mem>, ()> {
        let (store, deleted) = self.on_fork(w, |rows| match self.guards {
            Guards::NoWorkerCheck => {
                let txn = ExecRows::delete_job_txn(held, JOB)?;
                rows.commit(without_checks(&txn, "exec/w/", is_version))
                    .map(drop)
            }
            _ => rows.delete_job(held, JOB),
        });
        deleted.map(|()| store).map_err(drop)
    }

    /// A claim, the registration with the farm that follows it, and the rebuild of the
    /// frontend's tasks from the job rows of the worker.
    fn claim_steps(&self, w: &World, i: usize, out: &mut Vec<(String, World)>) {
        let name = |step: &str| format!("{step}({i})");
        let fe = &w.fes[i];
        let me = instance(i);
        if let (None, Some(live), true) = (&fe.held, &fe.live, w.claims < MAX_GENERATION) {
            let entries = w.rows(&w.store).workers().expect("workers");
            if let Some(entry) = entries.iter().find(|e| e.worker == WORKER) {
                let (store, claimed) = self.on_fork(w, |rows| match self.guards {
                    Guards::NoOwnerCheck => {
                        let txn = ExecRows::claim_txn(live, entry, &format!("farm-{me}"))?;
                        let txn = without_checks(&txn, "exec/fe/", is_absent);
                        let commit = rows.commit(txn)?;
                        Ok(Held {
                            worker: WORKER.to_owned(),
                            version: Version(commit.revision.0),
                        })
                    }
                    _ => rows.claim(live, entry, &format!("farm-{me}")),
                });
                if let Ok(held) = claimed {
                    let mut next = w.clone();
                    next.store = store;
                    next.fes[i].held = Some(held);
                    next.fes[i].task = Task::Adopting;
                    next.claims += 1;
                    out.push((name("claim"), next));
                }
            }
        }
        if fe.task == Task::Adopting {
            self.register(w, i, out);
        }
    }

    /// The registration of the adopted worker (a `Hello` that takes the farm's owner
    /// counter), then what the rows say about the job.
    fn register(&self, w: &World, i: usize, out: &mut Vec<(String, World)>) {
        let name = |step: &str| format!("{step}({i})");
        let fe = &w.fes[i];
        let mut next = w.clone();
        next.farm += 1;
        next.fes[i].farm = next.farm;
        let jobs = w.rows(&w.store).jobs(WORKER).expect("jobs");
        if jobs.is_empty() {
            // No row: the farm requeues the lease if no result was taken.
            if next.result == Outcome::None {
                next.result = Outcome::Requeued;
            }
            next.fes[i].task = Task::None;
            out.push((name("register_without_row"), next));
        } else if w.service != Service::None {
            next.fes[i].task = Task::Run;
            out.push((name("register_found_job"), next));
        } else {
            // The submit never landed: the lease is settled as an infrastructure failure.
            let held = fe.held.as_ref().expect("adopting holds the worker");
            match self.delete(w, held) {
                Ok(store) => {
                    next.store = store;
                    next.bad |= !w.holds_row(i);
                    next.done = true;
                    if next.result == Outcome::None {
                        next.result = Outcome::Infra;
                    }
                    next.fes[i].task = Task::None;
                    out.push((name("register_lost_job"), next));
                }
                Err(()) => {
                    let mut lost = w.lost(i);
                    lost.farm = next.farm;
                    lost.fes[i].farm = next.farm;
                    out.push((name("register_then_lost"), lost));
                }
            }
        }
    }
}

struct Report {
    states: usize,
    violations: usize,
    stuck: usize,
    /// The steps to the first state that broke a property, or empty.
    trace: Vec<String>,
}

fn search(config: &Search) -> Report {
    let start = World::start(config.n);
    let mut ids: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut parents: Vec<(usize, String)> = vec![(0, String::new())];
    let mut edges: Vec<Vec<usize>> = Vec::new();
    let mut goal: Vec<bool> = Vec::new();
    let mut capped: Vec<bool> = Vec::new();
    let mut violations = 0;
    let mut first = None;
    ids.insert(start.key(), 0);
    let mut queue = VecDeque::from([(0, start)]);
    while let Some((id, world)) = queue.pop_front() {
        if world.violation(config.fenced).is_some() {
            violations += 1;
            first.get_or_insert(id);
        }
        goal.push(world.result != Outcome::None);
        capped.push(world.claims >= MAX_GENERATION);
        let mut next_ids = Vec::new();
        for (step, next) in config.successors(&world) {
            let key = next.key();
            let next_id = *ids.entry(key).or_insert_with_key(|_| {
                let fresh = parents.len();
                parents.push((id, step.clone()));
                queue.push_back((fresh, next));
                fresh
            });
            next_ids.push(next_id);
        }
        debug_assert_eq!(edges.len(), id);
        edges.push(next_ids);
    }

    // The states from which a result is reachable: a backward walk from the results.
    let mut back: Vec<Vec<usize>> = vec![Vec::new(); edges.len()];
    for (from, tos) in edges.iter().enumerate() {
        for to in tos {
            back[*to].push(from);
        }
    }
    let mut reaches = goal.clone();
    let mut work: Vec<usize> = (0..goal.len()).filter(|s| goal[*s]).collect();
    while let Some(state) = work.pop() {
        for from in &back[state] {
            if !reaches[*from] {
                reaches[*from] = true;
                work.push(*from);
            }
        }
    }
    let stuck = (0..reaches.len())
        .filter(|s| !reaches[*s] && !capped[*s])
        .count();

    let mut trace = Vec::new();
    let mut at = first;
    while let Some(id) = at.filter(|id| *id != 0) {
        trace.push(parents[id].1.clone());
        at = Some(parents[id].0);
    }
    trace.reverse();
    Report {
        states: edges.len(),
        violations,
        stuck,
        trace,
    }
}

fn run(n: usize, fenced: bool, guards: Guards) -> Report {
    search(&Search { n, fenced, guards })
}

fn assert_sound(report: &Report) {
    assert!(
        report.states > 1_000,
        "the search ended early: {} states",
        report.states
    );
    assert_eq!(report.violations, 0, "a property broke: {:?}", report.trace);
    assert_eq!(report.stuck, 0, "{} states reach no result", report.stuck);
}

/// Every interleaving of the protocol of one worker, one lease and two frontends, with the
/// frontend that holds the worker stopping before its liveness row can expire, reaches a
/// result from every state and breaks no property.
#[test]
fn every_interleaving_reaches_a_result_at_two_frontends() {
    assert_sound(&run(2, true, Guards::All));
}

/// The same with a holder that may stall past its fence: expiry any time. At most one
/// holder is then not promised, but no job row is written without the worker row and a
/// result is still reachable.
#[test]
fn every_interleaving_reaches_a_result_at_two_frontends_when_a_holder_stalls() {
    assert_sound(&run(2, false, Guards::All));
}

/// The same at three frontends: a second update begins while the first still drains.
#[test]
fn every_interleaving_reaches_a_result_at_three_frontends() {
    assert_sound(&run(3, true, Guards::All));
}

#[test]
fn every_interleaving_reaches_a_result_at_three_frontends_when_a_holder_stalls() {
    assert_sound(&run(3, false, Guards::All));
}

/// A claim that does not check the owner's liveness row lets a second frontend hold the
/// worker while the first acts: the search finds it, and it is the claim that does it.
#[test]
fn a_claim_that_does_not_check_the_owners_liveness_row_lets_two_frontends_act() {
    let report = run(2, true, Guards::NoOwnerCheck);
    assert!(
        report.violations > 0,
        "the search did not see two frontends act"
    );
    assert_eq!(report.trace, ["claim(1)"]);
}

/// A job-row write that does not check the worker row lets a frontend that stalled past
/// its fence write after another claimed.
#[test]
fn a_job_write_that_does_not_check_the_worker_row_is_found_after_another_claim() {
    let report = run(2, false, Guards::NoWorkerCheck);
    assert!(report.violations > 0, "the search did not see the write");
    assert_eq!(report.trace, ["expire(0)", "claim(1)", "insert_job(0)"]);
}
