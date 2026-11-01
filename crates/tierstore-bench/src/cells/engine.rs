//! The machinery every cell shares: passes, arms, the load gate, the budget, the samples and the
//! result files.
//!
//! A cell builds a list of [`Arm`]s, each a unit of [`Work`] repeated `requests` times at a pace
//! (closed loop with `in_flight` workers, or open loop at a fixed rate), and hands them to
//! [`run_cell`]. The engine then keeps the rules of [`crate::cells`]:
//!
//! - passes: `passes` of every arm, pass 0 the discarded warm-up, arms interleaved (the order
//!   starts one arm later each pass, and is written into `notes`);
//! - the load gate is read before each pass and every few requests in it; a trip ends the pass,
//!   marks its samples busy and ends the run (the passes so far are written first);
//! - the byte ledger admits a cell before it sends anything and is charged what each pass
//!   wrote to the cold store;
//! - the cold store's state is read again before every pass and stamped into the results, and
//!   the cold store guard ([`crate::cells::coldguard`]) watches the harness's own cold requests as
//!   they complete: a trip ends the pass and the run, as a trip of the load gate does;
//! - the CSV of samples has one row per counted step, and one result JSON per arm and attempt
//!   (`<cell>-<arm>-a<N>.json`), never written over an existing one;
//! - a percentile of a pass is of the requests that succeeded; the p99 of a pass is filled in
//!   only for a quiet pass of 1,000 successes or more.
//!
//! What a step is: one line of a result. A request of a cell that is several requests of the
//! store (a multipart put) is one step whose latency is the whole; the requests inside it are
//! in the request log only.

use std::collections::BTreeMap;
use std::fs::File;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::cells::budget::{Ledger, Throttle};
use crate::cells::coldguard::ColdGuard;
use crate::cells::stats;
use crate::cells::streamget::{StreamGet, Streamed};
use crate::cells::{CellContext, CellId};
use crate::coldstate::ColdStateSource;
use crate::config::RunConfig;
use crate::error::BenchError;
use crate::gate::{LoadSample, Trip};
use crate::procload::{disk_busy_over, disk_busy_snapshot};
use crate::proxy::FaultMode;
use crate::requestlog::RequestLog;
use crate::result::{
    CellResult, ColdTrip, Op, PassColdState, PassResult, Percentiles, Provenance, ProxyRequests,
    SCHEMA_VERSION, Sample, Window,
};
use crate::role::Role;
use crate::s3::{BenchS3, Outcome, Timeouts, unix_ms};

/// Completions between two reads of the load gate, at most.
const GATE_EVERY: u64 = 16;

/// A boxed future of a cell's work.
pub type Boxed<T> = Pin<Box<dyn Future<Output = Result<T, BenchError>> + Send>>;

/// One request of an arm: `index` counts from 0 within the pass.
#[derive(Clone, Copy, Debug)]
pub struct Unit {
    /// The pass, from 0.
    pub pass: u32,
    /// The request within the pass.
    pub index: u64,
}

/// The work of an arm: what one [`Unit`] does, as steps.
pub type Work = Arc<dyn Fn(Unit) -> Boxed<Vec<Step>> + Send + Sync>;

/// A thing done before or after a pass of an arm (set a fault, delete what the pass wrote); it
/// gives notes for the result, or none.
pub type Hook = Arc<dyn Fn(u32) -> Boxed<Vec<String>> + Send + Sync>;

/// Wraps a closure as [`Work`].
pub fn work<F, Fut>(f: F) -> Work
where
    F: Fn(Unit) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Step>, BenchError>> + Send + 'static,
{
    Arc::new(move |unit| Box::pin(f(unit)))
}

/// Wraps a closure as a [`Hook`].
pub fn hook<F, Fut>(f: F) -> Hook
where
    F: Fn(u32) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<String>, BenchError>> + Send + 'static,
{
    Arc::new(move |pass| Box::pin(f(pass)))
}

/// What the machine looks like, for the load gate. The real source reads `/proc`
/// ([`ProcLoad`]); a test scripts it.
pub trait LoadSource: Send + Sync {
    /// The sample now.
    ///
    /// # Errors
    /// [`BenchError`] if the machine cannot be read; the run stops, since a gate that cannot see
    /// is not a gate.
    fn sample(&self) -> Result<LoadSample, BenchError>;
    /// The sample before a pass, over a quiet window of its own, so that the pass that just ran
    /// is not read as the machine's load. A source with no window says [`LoadSource::sample`].
    ///
    /// # Errors
    /// As [`LoadSource::sample`].
    fn quiet_sample(&self) -> Result<LoadSample, BenchError> {
        self.sample()
    }
    /// Milliseconds each whole disk of the machine has spent doing IO so far, by device name;
    /// empty for a source that cannot say.
    ///
    /// # Errors
    /// [`BenchError`] if the machine cannot be read.
    fn disk_busy_ms(&self) -> Result<BTreeMap<String, u64>, BenchError> {
        Ok(BTreeMap::new())
    }
}

/// The machine this process runs on, through [`LoadSample::read`].
pub struct ProcLoad;

impl LoadSource for ProcLoad {
    fn sample(&self) -> Result<LoadSample, BenchError> {
        LoadSample::read()
    }

    fn disk_busy_ms(&self) -> Result<BTreeMap<String, u64>, BenchError> {
        disk_busy_snapshot(Path::new("/proc"), Path::new("/sys/block"))
    }
}

/// What `now` has over `before`, by method: the requests made between two readings of the
/// proxy's counts. A method with none is left out; a count that went back counts as none.
fn count_delta(
    before: &BTreeMap<String, u64>,
    now: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    now.iter()
        .filter_map(|(method, n)| {
            let made = n.saturating_sub(before.get(method).copied().unwrap_or(0));
            (made > 0).then(|| (method.clone(), made))
        })
        .collect()
}

/// The fault proxy as a cell sees it: it sets the fault between passes and counts what went
/// through. [`crate::faultfile::FileSwitch`] implements it for a proxy process.
pub trait FaultSwitch: Send + Sync {
    /// From now on, the fault of `mode`.
    ///
    /// # Errors
    /// [`BenchError`] if the proxy cannot be told or does not take the mode: the arm must not
    /// be measured as if it had.
    fn set_mode(&self, mode: FaultMode) -> Result<(), BenchError>;
    /// Requests that went through the proxy so far.
    fn request_count(&self) -> u64;
    /// The same requests by HTTP method, so far; empty for a switch that cannot say.
    fn method_counts(&self) -> BTreeMap<String, u64> {
        BTreeMap::new()
    }
}

/// How an arm sends its requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// `in_flight` workers, each sending its next request when the last one is answered.
    Closed,
    /// One request every `1/per_sec` seconds whether or not the last one is answered, so a stall
    /// of the store shows as latency and not as a pause of the sender. The latency of a request
    /// that went out later than its turn includes the delay.
    Open {
        /// Requests per second.
        per_sec: u32,
    },
}

/// One line of a result: a request of the store, or the whole of several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// `""` for the arm itself; else a suffix (`.ttfb`) naming a second quantity of the same
    /// requests, which gets an arm and a result file of its own.
    pub label: &'static str,
    /// Whether this step is a second measurement of a request another step already stands for
    /// (the time to the first byte of a read, `.ttfb`), not a request of its own. A label alone
    /// does not say so: `.cold-direct` and `.second` are whole requests.
    pub second_quantity: bool,
    /// The store the request went to.
    pub role: Role,
    /// What it did.
    pub op: Op,
    /// Payload bytes.
    pub bytes: u64,
    /// When it was sent.
    pub started_unix_ms: u64,
    /// How long it took, microseconds.
    pub latency_us: u64,
    /// The HTTP status, if there was an answer.
    pub status: Option<u16>,
    /// Whether it succeeded.
    pub ok: bool,
    /// Bytes this step wrote to the cold store, for the ledger (a put that failed counts: it
    /// may have landed).
    pub cold_write: u64,
    /// Requests in flight when it was sent, set by the engine.
    pub in_flight: u32,
}

impl Step {
    /// The step of a timed request.
    #[must_use]
    pub fn of<T>(role: Role, op: Op, bytes: u64, outcome: &Outcome<T>) -> Self {
        Self {
            label: "",
            second_quantity: false,
            role,
            op,
            bytes,
            started_unix_ms: outcome.started_unix_ms,
            latency_us: outcome.latency_us(),
            status: outcome.status,
            ok: outcome.is_ok(),
            cold_write: 0,
            in_flight: 0,
        }
    }

    /// The step of a streamed read that was to give `expected` bytes: it succeeded only if it
    /// gave them all.
    #[must_use]
    pub fn streamed(role: Role, op: Op, expected: u64, read: &Streamed) -> Self {
        Self {
            label: "",
            second_quantity: false,
            role,
            op,
            bytes: read.bytes,
            started_unix_ms: read.started_unix_ms,
            latency_us: micros(read.total),
            status: read.status,
            ok: read.is_ok() && read.bytes == expected,
            cold_write: 0,
            in_flight: 0,
        }
    }

    /// The same step counted as `bytes` written to the cold store.
    #[must_use]
    pub fn writing(mut self, bytes: u64) -> Self {
        self.cold_write = bytes;
        self
    }

    /// The same step under `label`, a result of its own. It is still a request.
    #[must_use]
    pub fn labelled(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }

    /// The same step as a second measurement of a request shown by another step, under `label`:
    /// the cold store guard does not count it.
    #[must_use]
    pub fn second_quantity_of_request(mut self, label: &'static str) -> Self {
        self.label = label;
        self.second_quantity = true;
        self
    }
}

/// One arm of a cell.
pub struct Arm {
    /// The name of the arm: `a-z`, `0-9`, `-` (and `.` before a step suffix). It is in object
    /// keys, in the file name of the result and in the CSV.
    pub label: String,
    /// How requests are sent.
    pub pace: Pace,
    /// Workers of a closed loop.
    pub in_flight: u32,
    /// Requests per pass.
    pub requests: u64,
    /// Bytes one pass writes to the cold store, for the ledger.
    pub cold_bytes_per_pass: u64,
    /// Bytes the arm's setup writes once (objects to read), for the admission of the cell; the
    /// setup charges them itself ([`Charger`]).
    pub cold_bytes_once: u64,
    /// What one request does.
    pub work: Work,
    /// Before each pass (after the gate has looked).
    pub before: Option<Hook>,
    /// After each pass, also one that the gate cut short.
    pub after: Option<Hook>,
    /// Whether the arm puts the cold store in a fault on purpose (through the fault proxy). The
    /// cold store guard is not shown the arm's cold requests: they are meant to be slow or to
    /// fail.
    pub faulted: bool,
    /// Prose of the cell's for every result of the arm.
    pub notes: Vec<String>,
    /// The fault proxy in the path of the arm's cold requests, if there is one: its counts by
    /// method around each pass go into the pass result.
    pub proxy: Option<Arc<dyn FaultSwitch>>,
}

/// Which rate cap a client is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// The cap on requests to the cold store.
    Cold,
    /// The cap on requests to the hot store.
    Hot,
}

/// The samples file: one CSV row per counted step.
struct Recorder {
    path: PathBuf,
    file: Mutex<File>,
}

impl Recorder {
    fn open(dir: &Path) -> Result<Self, BenchError> {
        let path = dir.join("samples.csv");
        let io = |source| BenchError::Io {
            path: path.clone(),
            source,
        };
        let mut file = File::options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        if file.metadata().map_err(io)?.len() == 0 {
            writeln!(file, "{}", Sample::CSV_HEADER).map_err(io)?;
        }
        Ok(Self {
            path: path.clone(),
            file: Mutex::new(file),
        })
    }

    fn append(&self, rows: &[String]) -> Result<(), BenchError> {
        let mut text = rows.join("\n");
        text.push('\n');
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        file.write_all(text.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|source| BenchError::Io {
                path: self.path.clone(),
                source,
            })
    }
}

/// The ledger, as a cell's setup charges what it writes (objects it makes to read).
#[derive(Clone)]
pub struct Charger(Arc<Mutex<Ledger>>);

impl Charger {
    /// Adds `bytes` to what the campaign has written to the cold store.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the ledger cannot be saved.
    pub fn charge(&self, bytes: u64) -> Result<(), BenchError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .charge(bytes)
    }

    /// Counts `planned` bytes as written before they are sent (see [`Ledger::reserve`]).
    fn reserve(&self, planned: u64) -> Result<(), BenchError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reserve(planned)
    }

    /// Replaces a reservation by what was written (see [`Ledger::settle`]).
    fn settle(&self, planned: u64, actual: u64) -> Result<(), BenchError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .settle(planned, actual)
    }
}

/// What a run of cells holds: the configuration, the clients' shared parts, the ledger and the
/// files.
pub struct Env {
    /// The resolved configuration.
    pub config: RunConfig,
    /// Where the numbers come from, copied into every result and CSV row.
    pub provenance: Provenance,
    /// The harness-side request log.
    pub log: Arc<RequestLog>,
    /// Limits on a request's connect and whole life.
    pub timeouts: Timeouts,
    load: Arc<dyn LoadSource>,
    cold_state: Arc<dyn ColdStateSource>,
    guard: Mutex<ColdGuard>,
    cold_cap: Arc<Throttle>,
    hot_cap: Arc<Throttle>,
    recorder: Recorder,
    ledger: Charger,
}

impl Env {
    /// Opens the result directory (made if it is not there), the samples file and the campaign
    /// ledger, which must exist already (`Campaign::begin` makes it).
    ///
    /// # Errors
    /// [`BenchError`] if the directory or a file in it cannot be made or read, or there is no
    /// ledger.
    pub fn new(ctx: &CellContext, load: Arc<dyn LoadSource>) -> Result<Self, BenchError> {
        let dir = &ctx.config.result_dir;
        std::fs::create_dir_all(dir).map_err(|source| BenchError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self {
            config: ctx.config.clone(),
            provenance: ctx.provenance.clone(),
            log: ctx.log.clone(),
            timeouts: Timeouts::default(),
            load,
            cold_state: ctx.cold_state.clone(),
            guard: Mutex::new(ColdGuard::new(ctx.config.cold_guard)),
            cold_cap: Arc::new(Throttle::new(ctx.config.rates.cold_rps)),
            hot_cap: Arc::new(Throttle::new(ctx.config.rates.hot_rps)),
            recorder: Recorder::open(dir)?,
            ledger: Charger(Arc::new(Mutex::new(Ledger::open(
                &ctx.campaign.ledger,
                ctx.config.bytes,
            )?))),
        })
    }

    fn cap(&self, cap: Cap) -> Arc<Throttle> {
        match cap {
            Cap::Cold => self.cold_cap.clone(),
            Cap::Hot => self.hot_cap.clone(),
        }
    }

    /// A client of the bucket of `role`, held to `cap`.
    ///
    /// # Errors
    /// [`BenchError`] if the run does not configure `role` or its address is refused.
    pub fn s3(&self, role: Role, cap: Cap) -> Result<Arc<BenchS3>, BenchError> {
        let spec = self.config.store(role)?;
        let client = BenchS3::new(role, spec, self.timeouts, self.log.clone())?;
        Ok(Arc::new(client.with_cap(self.cap(cap))))
    }

    /// A streaming reader of the bucket of `role`, held to `cap`.
    ///
    /// # Errors
    /// As [`Env::s3`].
    pub fn stream(&self, role: Role, cap: Cap) -> Result<Arc<StreamGet>, BenchError> {
        let spec = self.config.store(role)?;
        let client = StreamGet::new(role, spec, self.timeouts, self.log.clone())?;
        Ok(Arc::new(client.with_cap(self.cap(cap))))
    }

    /// The key prefix of everything this run writes: `tierstore-bench/<run id>`, the run id as
    /// letters, digits and dashes, so cleanup has one place to look.
    #[must_use]
    pub fn prefix(&self) -> String {
        let run = slug(&self.provenance.run_id);
        let run = if run.is_empty() { "unrecorded" } else { &run };
        format!("tierstore-bench/{run}")
    }

    /// Reads the load gate: the first limit the machine is over, if one.
    ///
    /// # Errors
    /// [`BenchError`] if the machine cannot be read.
    pub fn gate(&self) -> Result<Option<Trip>, BenchError> {
        Ok(self.config.gate.check(&self.load.sample()?).err())
    }

    /// Shows the cold store guard the cold requests among `steps`, which finished now, and judges
    /// its window: the trip, if the cold store's health is over a limit.
    ///
    /// A request is one cold step, whatever its label, except a second quantity of a request
    /// already shown (`.ttfb`, the time to the first byte of a read, which only a read that
    /// succeeded has).
    fn watch_cold(&self, steps: &[Step]) -> Option<ColdTrip> {
        let mut cold = steps
            .iter()
            .filter(|s| s.role == Role::Cold && !s.second_quantity)
            .peekable();
        cold.peek()?;
        let now = Instant::now();
        let mut guard = self.guard.lock().unwrap_or_else(PoisonError::into_inner);
        for step in cold {
            guard.record(now, step.latency_us, !step.ok);
        }
        guard.check(now).err()
    }

    /// The result file of `arm` of `cell` for this run's attempt.
    #[must_use]
    pub fn result_path(&self, cell: CellId, arm: &str) -> PathBuf {
        self.config
            .result_dir
            .join(result_name(cell, arm, self.provenance.attempt))
    }

    /// Refuses the run, before anything is sent, if a result file of one of `arms` for this
    /// attempt is there already (an arm with a step suffix, `.ttfb`, is found too).
    fn refuse_existing(&self, cell: CellId, arms: &[Arm]) -> Result<(), BenchError> {
        let dir = &self.config.result_dir;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        let tail = format!("-a{}.json", self.provenance.attempt);
        let names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| name.ends_with(&tail))
            .collect();
        for arm in arms {
            let stem = format!("{}-{}", cell.as_str(), arm.label);
            let exact = format!("{stem}{tail}");
            let suffixed = format!("{stem}.");
            if let Some(found) = names
                .iter()
                .find(|n| **n == exact || n.starts_with(&suffixed))
            {
                return Err(BenchError::ResultExists {
                    path: dir.join(found),
                });
            }
        }
        Ok(())
    }

    fn admit(&self, pass_cost: u64, cell_cost: u64) -> Result<(), BenchError> {
        let ledger = self.ledger.0.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.admit(pass_cost, cell_cost)
    }

    /// The handle a cell's setup charges what it writes through.
    #[must_use]
    pub fn charger(&self) -> Charger {
        self.ledger.clone()
    }

    /// Bytes written to the cold store by the campaign so far.
    #[must_use]
    pub fn spent(&self) -> u64 {
        let ledger = self.ledger.0.lock().unwrap_or_else(PoisonError::into_inner);
        ledger.spent()
    }
}

/// The file name of the result of `arm` of `cell` for `attempt`: `<cell>-<arm>-a<N>.json`.
#[must_use]
pub fn result_name(cell: CellId, arm: &str, attempt: u32) -> String {
    format!("{}-{arm}-a{attempt}.json", cell.as_str())
}

/// `text` with every character that is not `a-z`, `0-9` or `-` replaced by `-` (letters
/// lowered), for a key or a label.
#[must_use]
pub fn slug(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// `duration` in microseconds, saturating.
#[must_use]
pub fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// The microseconds a request that was due at `due` was late when it went out at `now`.
fn lateness_us(due: Instant, now: Instant) -> u64 {
    micros(now.saturating_duration_since(due))
}

/// One request of a pass, counted in flight while it runs, its steps stamped.
async fn run_unit(
    work: &Work,
    unit: Unit,
    inflight: &AtomicU32,
    late_us: u64,
) -> Result<Vec<Step>, BenchError> {
    let at_start = inflight.fetch_add(1, Ordering::SeqCst) + 1;
    let result = work(unit).await;
    inflight.fetch_sub(1, Ordering::SeqCst);
    result.map(|mut steps| {
        for step in &mut steps {
            step.in_flight = at_start;
            step.latency_us = step.latency_us.saturating_add(late_us);
        }
        steps
    })
}

/// What a pass did.
struct Driven {
    steps: Vec<Step>,
    elapsed: Duration,
    trip: Option<Trip>,
    cold_trip: Option<ColdTrip>,
}

/// What ended a run early: the load gate, or the cold store guard.
enum Stop {
    Gate(Trip),
    Cold(ColdTrip),
}

impl From<Stop> for BenchError {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Gate(trip) => BenchError::Gate(trip),
            Stop::Cold(trip) => BenchError::ColdGuard(trip),
        }
    }
}

/// Sends the requests of one pass of `arm` and collects their steps, reading the gate as they
/// complete.
async fn drive(env: &Env, arm: &Arm, pass: u32) -> Result<Driven, BenchError> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Result<Vec<Step>, BenchError>>();
    let stop = Arc::new(AtomicBool::new(false));
    let inflight = Arc::new(AtomicU32::new(0));
    let clock = Instant::now();
    let mut tasks = JoinSet::new();
    match arm.pace {
        Pace::Closed => {
            let next = Arc::new(AtomicU64::new(0));
            let workers = u64::from(arm.in_flight.max(1)).min(arm.requests);
            for _ in 0..workers {
                let (tx, stop, inflight, next) =
                    (tx.clone(), stop.clone(), inflight.clone(), next.clone());
                let (work, requests) = (arm.work.clone(), arm.requests);
                tasks.spawn(async move {
                    loop {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        if index >= requests || stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let unit = Unit { pass, index };
                        let _ = tx.send(run_unit(&work, unit, &inflight, 0).await);
                    }
                });
            }
        }
        Pace::Open { per_sec } => {
            let (tx, stop, inflight) = (tx.clone(), stop.clone(), inflight.clone());
            let (work, requests) = (arm.work.clone(), arm.requests);
            tasks.spawn(async move {
                let gap = Duration::from_secs(1) / per_sec.max(1);
                let mut sent = JoinSet::new();
                for index in 0..requests {
                    let due = clock + gap * u32::try_from(index).unwrap_or(u32::MAX);
                    tokio::time::sleep_until(due).await;
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let late = lateness_us(due, Instant::now());
                    let (work, tx, inflight) = (work.clone(), tx.clone(), inflight.clone());
                    sent.spawn(async move {
                        let unit = Unit { pass, index };
                        let _ = tx.send(run_unit(&work, unit, &inflight, late).await);
                    });
                }
                while sent.join_next().await.is_some() {}
            });
        }
    }
    drop(tx);
    let every = (arm.requests / 4).clamp(1, GATE_EVERY);
    let mut steps = Vec::new();
    let mut done = 0u64;
    let mut trip = None;
    let mut cold_trip = None;
    while let Some(result) = rx.recv().await {
        let got = result?;
        // `&`: both are plain reads of a flag and an option.
        if !arm.faulted & cold_trip.is_none() {
            cold_trip = env.watch_cold(&got);
            if cold_trip.is_some() {
                stop.store(true, Ordering::SeqCst);
            }
        }
        steps.extend(got);
        done += 1;
        if done.is_multiple_of(every) & trip.is_none() {
            trip = env.gate()?;
            if trip.is_some() {
                stop.store(true, Ordering::SeqCst);
            }
        }
    }
    while tasks.join_next().await.is_some() {}
    Ok(Driven {
        steps,
        elapsed: clock.elapsed(),
        trip,
        cold_trip,
    })
}

/// The `PassResult` of the steps of one label of one pass.
fn pass_result(
    pass: u32,
    window: Window,
    elapsed: Duration,
    trip: Option<&Trip>,
    cold_trip: Option<&ColdTrip>,
    steps: &[&Step],
) -> PassResult {
    let mut latencies: Vec<u64> = steps
        .iter()
        .filter(|s| s.ok)
        .map(|s| s.latency_us)
        .collect();
    let claim = window == Window::Quiet && stats::p99_claimable(latencies.len());
    let percentiles = stats::summarise(&mut latencies).map(|s| Percentiles {
        p50_us: s.p50,
        p95_us: s.p95,
        p99_us: claim.then_some(s.p99),
        max_us: s.max,
    });
    PassResult {
        pass,
        window,
        discarded: pass == 0,
        requests: steps.len() as u64,
        failures: steps.iter().filter(|s| !s.ok).count() as u64,
        bytes: steps.iter().filter(|s| s.ok).map(|s| s.bytes).sum(),
        elapsed_us: micros(elapsed),
        percentiles,
        gate_tripped: trip.map(|t| t.metric.to_owned()),
        cold_guard: cold_trip.cloned(),
        proxy_requests: None,
        disk_busy: BTreeMap::new(),
        peak_in_flight: steps.iter().map(|s| s.in_flight).max().unwrap_or(0),
    }
}

/// What one arm has so far.
#[derive(Default)]
struct Acc {
    passes: Vec<PassResult>,
    notes: Vec<String>,
    cold_state: Vec<PassColdState>,
}

/// What the ledger is asked to admit for `arms` run for `passes` passes: the cost of the worst
/// pass of one arm, and of the cell in all (every arm's bytes per pass for every pass, and the
/// bytes its setup writes once).
#[must_use]
pub fn admission_cost(arms: &[Arm], passes: u32) -> (u64, u64) {
    let worst_pass = arms
        .iter()
        .map(|a| a.cold_bytes_per_pass.saturating_add(a.cold_bytes_once))
        .max()
        .unwrap_or(0);
    let once: u64 = arms.iter().map(|a| a.cold_bytes_once).sum();
    let writes: u64 = arms.iter().map(|a| a.cold_bytes_per_pass).sum();
    let total = writes
        .saturating_mul(u64::from(passes))
        .saturating_add(once);
    (worst_pass, total)
}

/// Runs the arms of a cell and writes its results; see the module docs. `cleanup`, if given, runs
/// once after the last pass, also after a trip of the gate or an error, and its notes go to every
/// result.
///
/// # Errors
/// [`BenchError::ResultExists`] before anything is sent if a result file of this attempt is
/// there; [`BenchError::Budget`] before anything is sent if the ledger refuses the cell;
/// [`BenchError::Gate`] or [`BenchError::ColdGuard`] after the results so far are written if the
/// load gate or the cold store guard tripped; any error of a
/// piece of work, a hook, the files or the log stops the run at once (the cleanup still runs).
pub async fn run_cell(
    env: &Env,
    cell: CellId,
    arms: Vec<Arm>,
    passes: u32,
    cleanup: Option<Hook>,
) -> Result<Vec<CellResult>, BenchError> {
    env.refuse_existing(cell, &arms)?;
    let (worst_pass, cell_total) = admission_cost(&arms, passes);
    env.admit(worst_pass, cell_total)?;
    let started_unix_ms = unix_ms();
    let mut acc: BTreeMap<String, Acc> = arms
        .iter()
        .map(|a| {
            let notes = a.notes.clone();
            (
                a.label.clone(),
                Acc {
                    passes: vec![],
                    notes,
                    cold_state: vec![],
                },
            )
        })
        .collect();
    let mut order = Vec::new();
    let ran = run_passes(env, cell, &arms, passes, &mut acc, &mut order).await;
    let cleaned = match &cleanup {
        Some(cleanup) => cleanup(0).await,
        None => Ok(vec![]),
    };
    let stopped = ran?;
    let cleaned = cleaned?;
    let finished_unix_ms = unix_ms();
    let mut results = Vec::new();
    for (label, got) in acc {
        let mut notes = got.notes;
        notes.push(format!(
            "arms interleaved, the order of each pass: {}",
            order.join("; ")
        ));
        notes.extend(cleaned.iter().cloned());
        let result = CellResult {
            schema_version: SCHEMA_VERSION,
            cell,
            arm: label,
            started_unix_ms,
            finished_unix_ms,
            provenance: env.provenance.clone(),
            passes: got.passes,
            notes,
            cold_state: got.cold_state,
            rate_caps: Some(env.config.rates),
        };
        write_new(&env.result_path(cell, &result.arm), &result.to_json())?;
        results.push(result);
    }
    match stopped {
        None => Ok(results),
        Some(stop) => Err(stop.into()),
    }
}

/// Writes `text` to a new file `path`; a file that is there is never replaced.
pub(crate) fn write_new(path: &Path, text: &str) -> Result<(), BenchError> {
    let io = |source| BenchError::Io {
        path: path.to_owned(),
        source,
    };
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                BenchError::ResultExists {
                    path: path.to_owned(),
                }
            } else {
                io(source)
            }
        })?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .map_err(io)
}

/// The passes of every arm, interleaved; gives what ended them early, if something did.
async fn run_passes(
    env: &Env,
    cell: CellId,
    arms: &[Arm],
    passes: u32,
    acc: &mut BTreeMap<String, Acc>,
    order: &mut Vec<String>,
) -> Result<Option<Stop>, BenchError> {
    for pass in 0..passes {
        let turn: Vec<&Arm> = (0..arms.len())
            .map(|k| &arms[(k + pass as usize) % arms.len()])
            .collect();
        order.push(format!(
            "pass {pass}: {}",
            turn.iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        for arm in turn {
            if let Some(stop) = run_pass(env, cell, arm, pass, acc).await? {
                return Ok(Some(stop));
            }
        }
    }
    Ok(None)
}

/// One pass of one arm; gives what stopped it (the load gate before or during, the cold store
/// guard during), if something did.
async fn run_pass(
    env: &Env,
    cell: CellId,
    arm: &Arm,
    pass: u32,
    acc: &mut BTreeMap<String, Acc>,
) -> Result<Option<Stop>, BenchError> {
    if let Some(trip) = env.gate()? {
        let empty = pass_result(pass, Window::Busy, Duration::ZERO, Some(&trip), None, &[]);
        acc.entry(arm.label.clone()).or_default().passes.push(empty);
        return Ok(Some(Stop::Gate(trip)));
    }
    // Read again before every pass, not once at preflight: a heal can start or end in a campaign
    // of hours. The read is one short command of this process; nothing else runs meanwhile.
    let cold_state = env
        .cold_state
        .read_within(env.config.cold_state_timeout)
        .await?;
    // The pass's planned bytes are counted before anything is sent, so a run that dies in the
    // middle of a pass, or a ledger read by another run meanwhile, never sees fewer bytes than
    // may have landed. After the pass the count becomes what was written (a failed put counts).
    let planned = arm.cold_bytes_per_pass;
    env.ledger.reserve(planned)?;
    let mut notes = Vec::new();
    if let Some(before) = &arm.before {
        notes.extend(before(pass).await?);
    }
    // Read after the `before` hook (its setup is not the pass) and right around the requests.
    let disks_before = env.load.disk_busy_ms()?;
    let proxy_before = arm.proxy.as_ref().map(|p| p.method_counts());
    let driven = drive(env, arm, pass).await?;
    let disks_after = env.load.disk_busy_ms()?;
    let proxy_driven = arm.proxy.as_ref().map(|p| p.method_counts());
    if let Some(after) = &arm.after {
        notes.extend(after(pass).await?);
    }
    let proxy_requests = match (proxy_before, proxy_driven, &arm.proxy) {
        (Some(before), Some(sent), Some(proxy)) => Some(ProxyRequests {
            during: count_delta(&before, &sent),
            cleanup: count_delta(&sent, &proxy.method_counts()),
        }),
        _ => None,
    };
    let disk_busy = disk_busy_over(&disks_before, &disks_after, micros(driven.elapsed));
    env.ledger
        .settle(planned, driven.steps.iter().map(|s| s.cold_write).sum())?;
    let window = if driven.trip.is_some() || driven.cold_trip.is_some() {
        Window::Busy
    } else {
        env.config.window
    };
    let mut by_label: BTreeMap<&str, Vec<&Step>> = BTreeMap::from([("", vec![])]);
    for step in &driven.steps {
        by_label.entry(step.label).or_default().push(step);
    }
    let mut rows = Vec::new();
    for (suffix, steps) in by_label {
        let label = format!("{}{suffix}", arm.label);
        for step in &steps {
            rows.push(
                Sample {
                    cell,
                    arm: label.clone(),
                    pass,
                    window,
                    role: step.role,
                    op: step.op,
                    bytes: step.bytes,
                    in_flight: step.in_flight,
                    started_unix_ms: step.started_unix_ms,
                    latency_us: step.latency_us,
                    status: step.status,
                }
                .csv_row(&env.provenance),
            );
        }
        let result = pass_result(
            pass,
            window,
            driven.elapsed,
            driven.trip.as_ref(),
            driven.cold_trip.as_ref(),
            &steps,
        );
        let result = PassResult {
            proxy_requests: proxy_requests.clone(),
            disk_busy: disk_busy.clone(),
            ..result
        };
        let slot = acc.entry(label).or_default();
        slot.passes.push(result);
        slot.cold_state
            .extend(cold_state.iter().map(|state| PassColdState {
                pass,
                state: state.clone(),
            }));
        slot.notes
            .extend(notes.iter().map(|n| format!("pass {pass}: {n}")));
    }
    if !rows.is_empty() {
        env.recorder.append(&rows)?;
    }
    Ok(driven
        .trip
        .map(Stop::Gate)
        .or_else(|| driven.cold_trip.map(Stop::Cold)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::cells::testkit::{self, Script, busy, calm, loaded};

    fn temp(name: &str) -> kbf_testkit::dir::TestDir {
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), name)
    }

    fn env(dir: &Path, window: Window, load: Arc<dyn LoadSource>) -> Env {
        Env::new(&testkit::ctx(dir, &[], window), load).unwrap()
    }

    /// A successful step of `latency_us`, with `cold_write` bytes written.
    fn step(latency_us: u64, cold_write: u64) -> Step {
        Step {
            label: "",
            second_quantity: false,
            role: Role::Cold,
            op: Op::Put,
            bytes: 10,
            started_unix_ms: 5,
            latency_us,
            status: Some(200),
            ok: true,
            cold_write,
            in_flight: 0,
        }
    }

    /// An arm whose requests give one step each and record their index.
    fn arm(label: &str, requests: u64, in_flight: u32, seen: &Arc<Mutex<Vec<(u32, u64)>>>) -> Arm {
        let seen = seen.clone();
        Arm {
            label: label.to_owned(),
            pace: Pace::Closed,
            in_flight,
            requests,
            cold_bytes_per_pass: 0,
            cold_bytes_once: 0,
            work: work(move |unit| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push((unit.pass, unit.index));
                    tokio::task::yield_now().await;
                    Ok(vec![step(100 + unit.index, 0)])
                }
            }),
            before: None,
            after: None,
            faulted: false,
            proxy: None,
            notes: vec!["a note of the cell".to_owned()],
        }
    }

    fn csv_rows(dir: &Path) -> Vec<Vec<String>> {
        std::fs::read_to_string(dir.join("samples.csv"))
            .unwrap()
            .lines()
            .skip(1)
            .map(|l| l.split(',').map(str::to_owned).collect())
            .collect()
    }

    /// Catches a request sent twice or never, a pass left out of the result, the warm-up not
    /// marked discarded, a CSV row lost, and an in-flight count over the workers.
    #[tokio::test]
    async fn every_request_of_every_pass_is_sent_once_and_recorded_once() {
        let dir = temp("tb-engine-closed");
        let env = env(dir.path(), Window::Quiet, Script::calm());
        let seen = Arc::new(Mutex::new(vec![]));
        let results = run_cell(&env, CellId::ColdPut, vec![arm("a", 10, 3, &seen)], 3, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        let mut sent = seen.lock().unwrap().clone();
        sent.sort_unstable();
        let expected: Vec<(u32, u64)> = (0..3).flat_map(|p| (0..10).map(move |i| (p, i))).collect();
        assert_eq!(sent, expected);
        let passes = &results[0].passes;
        assert_eq!(passes.len(), 3);
        assert_eq!(
            passes.iter().map(|p| p.discarded).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert!(
            passes
                .iter()
                .all(|p| (p.requests, p.failures, p.bytes) == (10, 0, 100))
        );
        assert!(
            passes
                .iter()
                .all(|p| (p.window, p.gate_tripped.as_deref()) == (Window::Quiet, None))
        );
        let p = passes[1].percentiles.unwrap();
        assert_eq!((p.p50_us, p.max_us, p.p99_us), (104, 109, None));
        let rows = csv_rows(dir.path());
        assert_eq!(rows.len(), 30);
        assert!(
            rows.iter().all(
                |r| (r[3].as_str(), r[4].as_str(), r[6].as_str()) == ("cold-put", "a", "quiet")
            )
        );
        assert!(
            rows.iter()
                .all(|r| (1..=3).contains(&r[10].parse::<u32>().unwrap()))
        );
        let on_disk = std::fs::read_to_string(dir.path().join("cold-put-a-a1.json")).unwrap();
        assert_eq!(CellResult::from_json(&on_disk).unwrap(), results[0]);
    }

    /// Catches arms that are not interleaved (each pass starting with the same arm), an order
    /// that is not written down, a note of the cell lost, and a result file per arm.
    #[tokio::test]
    async fn arms_take_turns_to_go_first_and_the_order_is_in_the_notes() {
        let dir = temp("tb-engine-order");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let seen = Arc::new(Mutex::new(vec![]));
        let arms = vec![arm("a", 1, 1, &seen), arm("b", 1, 1, &seen)];
        let results = run_cell(&env, CellId::G, arms, 3, None).await.unwrap();
        assert_eq!(results.len(), 2);
        let notes = &results[0].notes;
        assert_eq!(notes[0], "a note of the cell");
        assert!(
            notes[1].ends_with("pass 0: a b; pass 1: b a; pass 2: a b"),
            "{notes:?}"
        );
        assert!(dir.path().join("g-a-a1.json").exists());
        assert!(dir.path().join("g-b-a1.json").exists());
        assert!(results[1].passes.iter().all(|p| p.window == Window::Busy));
    }

    /// Catches the quiet claim being made from a busy run, a p99 from fewer than a thousand
    /// successes, and a failure counted among the latencies.
    #[test]
    fn a_p99_is_claimed_from_a_quiet_pass_of_a_thousand_successes_only() {
        let many = |n: u64| (0..n).map(|i| step(i + 1, 0)).collect::<Vec<_>>();
        let run = |window, steps: &[Step]| {
            let refs: Vec<&Step> = steps.iter().collect();
            pass_result(1, window, Duration::from_secs(2), None, None, &refs)
        };
        let thousand = many(1000);
        let p = run(Window::Quiet, &thousand).percentiles.unwrap();
        assert_eq!(
            (p.p50_us, p.p95_us, p.p99_us, p.max_us),
            (500, 950, Some(990), 1000)
        );
        assert_eq!(
            run(Window::Quiet, &many(999)).percentiles.unwrap().p99_us,
            None
        );
        assert_eq!(
            run(Window::Busy, &thousand).percentiles.unwrap().p99_us,
            None
        );

        let mut with_failure = many(10);
        with_failure.push(Step {
            ok: false,
            status: None,
            latency_us: 999_999,
            ..step(0, 0)
        });
        let result = run(Window::Quiet, &with_failure);
        assert_eq!(
            (result.requests, result.failures, result.bytes),
            (11, 1, 100)
        );
        assert_eq!(result.percentiles.unwrap().max_us, 10);
        assert_eq!(result.elapsed_us, 2_000_000);
        let nothing = run(
            Window::Quiet,
            &[Step {
                ok: false,
                ..step(1, 0)
            }],
        );
        assert_eq!(nothing.percentiles, None);
    }

    /// Catches a gate that is not read before a pass: a busy machine at the start sends nothing,
    /// writes no sample, and leaves a pass that names the measure.
    #[tokio::test]
    async fn a_busy_machine_before_a_pass_sends_nothing_and_says_which_measure() {
        let dir = temp("tb-engine-gate-before");
        let env = env(dir.path(), Window::Quiet, Script::of(vec![Some(busy())]));
        let seen = Arc::new(Mutex::new(vec![]));
        let err = run_cell(&env, CellId::ColdPut, vec![arm("a", 4, 2, &seen)], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Gate(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "load gate: psi_cpu_avg10 is 99, over the limit 10"
        );
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(csv_rows(dir.path()).len(), 0);
        let written = std::fs::read_to_string(dir.path().join("cold-put-a-a1.json")).unwrap();
        let result = CellResult::from_json(&written).unwrap();
        assert_eq!(result.passes.len(), 1);
        assert_eq!(
            result.passes[0].gate_tripped.as_deref(),
            Some("psi_cpu_avg10")
        );
        assert_eq!(result.passes[0].window, Window::Busy);
        assert_eq!(result.passes[0].requests, 0);
    }

    /// Catches a gate that is not read during a pass: a machine that turns busy after two
    /// completions cuts the pass short, marks its samples busy though the run said quiet, ends
    /// the run, and still runs the arm's after hook.
    #[tokio::test]
    async fn a_machine_that_turns_busy_in_a_pass_cuts_it_and_ends_the_run() {
        let dir = temp("tb-engine-gate-during");
        let load = Script::of(vec![Some(calm()), Some(calm()), Some(busy())]);
        let env = env(dir.path(), Window::Quiet, load);
        let seen = Arc::new(Mutex::new(vec![]));
        let after_ran = Arc::new(AtomicUsize::new(0));
        let mut a = arm("a", 8, 1, &seen);
        let counter = after_ran.clone();
        a.after = Some(hook(move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            }
        }));
        let err = run_cell(&env, CellId::ColdPut, vec![a], 3, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Gate(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "load gate: psi_cpu_avg10 is 99, over the limit 10"
        );
        let written = std::fs::read_to_string(dir.path().join("cold-put-a-a1.json")).unwrap();
        let result = CellResult::from_json(&written).unwrap();
        assert_eq!(result.passes.len(), 1);
        let pass = &result.passes[0];
        assert!(pass.requests < 8, "{}", pass.requests);
        assert!(pass.requests >= 4, "{}", pass.requests);
        assert_eq!(pass.window, Window::Busy);
        assert_eq!(after_ran.load(Ordering::SeqCst), 1);
        assert!(csv_rows(dir.path()).iter().all(|r| r[6] == "busy"));
    }

    /// A machine whose reading during a pass (`sample`) and whose reading in the quiet window
    /// before one (`quiet_sample`) are scripted apart.
    struct Split {
        during: Arc<Script>,
        quiet: LoadSample,
    }

    impl LoadSource for Split {
        fn sample(&self) -> Result<LoadSample, BenchError> {
            self.during.sample()
        }

        fn quiet_sample(&self) -> Result<LoadSample, BenchError> {
            Ok(self.quiet)
        }
    }

    fn disk_hot() -> LoadSample {
        LoadSample {
            disk_util_pct: 100.0,
            ..calm()
        }
    }

    /// Catches a pass refused for the disk time of the pass before it: the reading over the
    /// interval since the last one is hot, the quiet window is idle, and the pass runs.
    #[tokio::test]
    async fn the_gate_before_a_pass_reads_the_quiet_window_not_the_last_pass() {
        let dir = temp("tb-engine-gate-quiet-idle");
        let load = Arc::new(Split {
            during: Script::of(vec![Some(disk_hot())]),
            quiet: calm(),
        });
        let env = env(dir.path(), Window::Quiet, load);
        let seen = Arc::new(Mutex::new(vec![]));
        run_cell(&env, CellId::ColdPut, vec![arm("a", 4, 2, &seen)], 1, None)
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 4);
    }

    /// Catches a gate before a pass that ignores other work: a hot quiet window sends nothing
    /// and names the disk.
    #[tokio::test]
    async fn a_busy_quiet_window_before_a_pass_sends_nothing_and_names_the_disk() {
        let dir = temp("tb-engine-gate-quiet-busy");
        let load = Arc::new(Split {
            during: Script::calm(),
            quiet: disk_hot(),
        });
        let env = env(dir.path(), Window::Quiet, load);
        let seen = Arc::new(Mutex::new(vec![]));
        let err = run_cell(&env, CellId::ColdPut, vec![arm("a", 4, 2, &seen)], 1, None)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "load gate: disk_util_pct is 100, over the limit 60"
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Catches the one-minute load average stopping a run: it carries the run's own in-flight
    /// requests and fsyncs for a minute, so a reading of 99 before and during a pass is not a
    /// reason to refuse or cut it.
    #[tokio::test]
    async fn the_load_average_does_not_stop_a_run_between_or_inside_passes() {
        let dir = temp("tb-engine-gate-loadavg");
        let load = Script::of(vec![Some(loaded()); 8]);
        let env = env(dir.path(), Window::Quiet, load);
        let seen = Arc::new(Mutex::new(vec![]));
        run_cell(&env, CellId::ColdPut, vec![arm("a", 4, 2, &seen)], 2, None)
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 8);
    }

    /// Catches a gate that cannot be read being taken for a calm machine.
    #[tokio::test]
    async fn a_machine_that_cannot_be_read_stops_the_run() {
        let dir = temp("tb-engine-gate-unreadable");
        let env = env(dir.path(), Window::Busy, Script::of(vec![None]));
        let seen = Arc::new(Mutex::new(vec![]));
        let err = run_cell(&env, CellId::G, vec![arm("a", 4, 1, &seen)], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::NotYet(_)));
        let dir = temp("tb-engine-gate-unreadable-mid");
        let env = env_with(dir.path(), Script::of(vec![Some(calm()), None]));
        let err = run_cell(&env, CellId::G, vec![arm("a", 4, 1, &seen)], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::NotYet(_)));
    }

    fn env_with(dir: &Path, load: Arc<dyn LoadSource>) -> Env {
        env(dir, Window::Busy, load)
    }

    /// An environment for the `attempt`-th try of a run whose results are in `dir`.
    fn env_attempt(dir: &Path, load: Arc<dyn LoadSource>, attempt: u32) -> Env {
        let mut ctx = testkit::ctx(dir, &[], Window::Busy);
        ctx.provenance.attempt = attempt;
        Env::new(&ctx, load).unwrap()
    }

    /// Catches a cell that is run when the ledger would be over: nothing is sent, nothing is
    /// charged, and the refusal names the cap.
    #[tokio::test]
    async fn the_ledger_refuses_a_cell_that_does_not_fit_before_it_sends_anything() {
        let dir = temp("tb-engine-budget");
        let mut ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        ctx.config.bytes = crate::config::ByteCaps {
            campaign: 1000,
            pass: 100,
            cell: 300,
        };
        let env = Env::new(&ctx, Script::calm()).unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let mut big = arm("a", 2, 1, &seen);
        big.cold_bytes_per_pass = 101;
        let err = run_cell(&env, CellId::ColdPut, vec![big], 2, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pass cap is 100"), "{err}");
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(env.spent(), 0);
    }

    /// Catches the bytes a pass wrote not being charged (a campaign would pass its cap), and a
    /// failed put not being charged, which may have landed.
    #[tokio::test]
    async fn what_a_pass_writes_is_charged_to_the_ledger_failures_included() {
        let dir = temp("tb-engine-charge");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let mut a = arm("a", 3, 1, &Arc::new(Mutex::new(vec![])));
        a.cold_bytes_per_pass = 30;
        a.work = work(|unit| async move {
            let mut s = step(1, 10);
            s.ok = unit.index != 0;
            Ok(vec![s])
        });
        run_cell(&env, CellId::ColdPut, vec![a], 2, None)
            .await
            .unwrap();
        assert_eq!(env.spent(), 60);
        let again = Env::new(&testkit::ctx(dir.path(), &[], Window::Busy), Script::calm()).unwrap();
        assert_eq!(again.spent(), 60);
        assert_eq!(csv_rows(dir.path()).len(), 6);
    }

    /// Catches a pass counted only after it is sent (a run that dies in the middle, or a second
    /// run reading the ledger meanwhile, would see too little): the first request of every pass
    /// finds the pass's planned bytes already in the ledger file; a pass that wrote less is then
    /// corrected down to what it wrote, and a pass that fails leaves its reservation counted.
    #[tokio::test]
    async fn a_pass_is_reserved_before_it_sends_and_settled_to_what_it_wrote() {
        let dir = temp("tb-engine-reserve");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let file = dir.path().join("ledger.json");
        let on_disk = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut a = arm("a", 1, 1, &Arc::new(Mutex::new(vec![])));
        a.cold_bytes_per_pass = 30;
        let (seen, path) = (on_disk.clone(), file.clone());
        a.work = work(move |_| {
            let (seen, path) = (seen.clone(), path.clone());
            async move {
                seen.lock()
                    .unwrap()
                    .push(std::fs::read_to_string(&path).unwrap());
                Ok(vec![step(1, 10)])
            }
        });
        run_cell(&env, CellId::ColdPut, vec![a], 2, None)
            .await
            .unwrap();
        assert_eq!(
            *on_disk.lock().unwrap(),
            ["{\"cold_written_bytes\":30}", "{\"cold_written_bytes\":40}"],
            "reserved 30 before the first pass; 10 written and 30 reserved again for the second"
        );
        assert_eq!(env.spent(), 20);

        let mut failing = arm("f", 1, 1, &Arc::new(Mutex::new(vec![])));
        failing.cold_bytes_per_pass = 30;
        failing.work = work(|_| async { Err(BenchError::Request("work")) });
        run_cell(&env, CellId::ColdPut, vec![failing], 2, None)
            .await
            .unwrap_err();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "{\"cold_written_bytes\":50}",
            "a pass that died keeps its reservation"
        );
    }

    /// Catches a campaign that starts without a ledger: with no ledger file, opening the cells'
    /// environment is refused and nothing is made.
    #[test]
    fn a_run_without_a_ledger_does_not_start() {
        let dir = temp("tb-engine-noledger");
        let mut ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        ctx.campaign.ledger = dir.path().join("elsewhere.json");
        let err = Env::new(&ctx, Script::calm()).err().unwrap();
        assert!(matches!(err, BenchError::Budget(_)), "{err}");
        assert!(!ctx.campaign.ledger.exists());
    }

    /// Catches an open loop that waits for answers (it must send on its own clock), loses a
    /// request, or ignores the gate: all requests of a calm run are sent, and a machine that
    /// turns busy stops the sending.
    #[tokio::test(start_paused = true)]
    async fn an_open_loop_sends_on_its_own_clock_and_stops_at_the_gate() {
        let dir = temp("tb-engine-open");
        let env = env(dir.path(), Window::Quiet, Script::calm());
        let seen = Arc::new(Mutex::new(vec![]));
        let mut a = arm("a", 20, 1, &seen);
        a.pace = Pace::Open { per_sec: 1000 };
        let results = run_cell(&env, CellId::A1, vec![a], 2, None).await.unwrap();
        assert_eq!(
            results[0]
                .passes
                .iter()
                .map(|p| p.requests)
                .collect::<Vec<_>>(),
            [20, 20]
        );

        let dir = temp("tb-engine-open-gate");
        let readings = vec![Some(calm()), Some(calm()), Some(calm()), Some(busy())];
        let env = env_with(dir.path(), Script::of(readings));
        let seen = Arc::new(Mutex::new(vec![]));
        let mut a = arm("a", 8, 1, &seen);
        a.pace = Pace::Open { per_sec: 1 };
        let err = run_cell(&env, CellId::A1, vec![a], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Gate(_)));
        assert_eq!(seen.lock().unwrap().len(), 6);
    }

    /// Catches a delay that is not added to the latency of an open-loop request that went out
    /// late (a stall of the sender would hide as a fast store), and one that is negative.
    #[test]
    fn lateness_is_the_time_past_the_turn_and_never_negative() {
        let now = Instant::now();
        assert_eq!(lateness_us(now, now + Duration::from_millis(3)), 3000);
        assert_eq!(lateness_us(now + Duration::from_millis(3), now), 0);
    }

    /// Catches hooks run out of order or with the wrong pass, and their notes not reaching the
    /// result.
    #[tokio::test]
    async fn hooks_run_around_each_pass_and_their_notes_reach_the_result() {
        let dir = temp("tb-engine-hooks");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut a = arm("a", 1, 1, &Arc::new(Mutex::new(vec![])));
        for (name, slot) in [("before", 0), ("after", 1)] {
            let events = events.clone();
            let h = hook(move |pass| {
                let events = events.clone();
                async move {
                    events.lock().unwrap().push(format!("{name} {pass}"));
                    Ok(vec![format!("{name} note")])
                }
            });
            if slot == 0 {
                a.before = Some(h);
            } else {
                a.after = Some(h);
            }
        }
        let results = run_cell(&env, CellId::ColdPut, vec![a], 2, None)
            .await
            .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            ["before 0", "after 0", "before 1", "after 1"]
        );
        let notes = &results[0].notes;
        assert!(notes.contains(&"pass 1: before note".to_owned()));
        assert!(notes.contains(&"pass 0: after note".to_owned()));
    }

    /// Catches a second quantity of the same requests lost, or filed under the arm itself: the
    /// steps labelled `.ttfb` make an arm and a result file of their own, with their own rows.
    #[tokio::test]
    async fn a_labelled_step_is_an_arm_of_its_own() {
        let dir = temp("tb-engine-labels");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let mut a = arm("read", 2, 1, &Arc::new(Mutex::new(vec![])));
        a.work = work(|_| async { Ok(vec![step(900, 0), step(40, 0).labelled(".ttfb")]) });
        let results = run_cell(&env, CellId::ColdRead, vec![a], 2, None)
            .await
            .unwrap();
        let names: Vec<&str> = results.iter().map(|r| r.arm.as_str()).collect();
        assert_eq!(names, ["read", "read.ttfb"]);
        assert_eq!(results[1].passes[1].percentiles.unwrap().p50_us, 40);
        assert!(dir.path().join("cold-read-read.ttfb-a1.json").exists());
        let rows = csv_rows(dir.path());
        assert_eq!(rows.iter().filter(|r| r[4] == "read.ttfb").count(), 4);
    }

    /// Catches an error of a piece of work being swallowed, an arm with no requests writing a
    /// blank CSV line, and an empty cell.
    #[tokio::test]
    async fn an_error_of_the_work_stops_the_run_and_an_arm_of_nothing_is_quiet() {
        let dir = temp("tb-engine-errors");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let mut failing = arm("a", 3, 2, &Arc::new(Mutex::new(vec![])));
        failing.work = work(|_| async { Err(BenchError::Request("the log is gone")) });
        let err = run_cell(&env, CellId::ColdPut, vec![failing], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Request("the log is gone")));

        let nothing = arm("b", 0, 4, &Arc::new(Mutex::new(vec![])));
        let results = run_cell(&env, CellId::ColdPut, vec![nothing], 2, None)
            .await
            .unwrap();
        assert_eq!(results[0].passes[1].requests, 0);
        assert_eq!(csv_rows(dir.path()).len(), 0);
        assert!(
            run_cell(&env, CellId::ColdPut, vec![], 2, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Catches a hook that fails being ignored, and a result or sample file that cannot be
    /// written being taken for a good run.
    #[tokio::test]
    async fn a_failing_hook_or_file_stops_the_run() {
        let dir = temp("tb-engine-io");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let mut a = arm("a", 1, 1, &Arc::new(Mutex::new(vec![])));
        a.before = Some(hook(|_| async { Err(BenchError::Request("hook")) }));
        assert!(run_cell(&env, CellId::G, vec![a], 2, None).await.is_err());
        let mut a = arm("a", 1, 1, &Arc::new(Mutex::new(vec![])));
        a.after = Some(hook(|_| async { Err(BenchError::Request("hook")) }));
        assert!(run_cell(&env, CellId::G, vec![a], 2, None).await.is_err());

        // A name taken (by anything) is a result that is there: the run does not start.
        std::fs::create_dir(dir.path().join("g-blocked-a1.json")).unwrap();
        let blocked = arm("blocked", 1, 1, &Arc::new(Mutex::new(vec![])));
        let err = run_cell(&env, CellId::G, vec![blocked], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::ResultExists { .. }), "{err}");

        // A result directory that has gone is a file that cannot be written.
        let gone = temp("tb-engine-gone");
        let sub = gone.path().join("sub");
        let gone_env = env_attempt(&sub, Script::calm(), 1);
        std::fs::remove_dir_all(&sub).unwrap();
        let a = arm("a", 1, 1, &Arc::new(Mutex::new(vec![])));
        let err = run_cell(&gone_env, CellId::G, vec![a], 2, None)
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Io { .. }), "{err}");

        let read_only = std::fs::File::open(dir.path().join("samples.csv")).unwrap();
        let recorder = Recorder {
            path: dir.path().join("samples.csv"),
            file: Mutex::new(read_only),
        };
        assert!(matches!(
            recorder.append(&["x".to_owned()]),
            Err(BenchError::Io { .. })
        ));
        let other = temp("tb-engine-io-dir");
        std::fs::create_dir(other.path().join("samples.csv")).unwrap();
        let ctx = testkit::ctx(other.path(), &[], Window::Busy);
        assert!(matches!(
            Env::new(&ctx, Script::calm()),
            Err(BenchError::Io { .. })
        ));
        let not_a_dir = other.path().join("file");
        std::fs::write(&not_a_dir, "x").unwrap();
        let mut ctx = testkit::ctx(other.path(), &[], Window::Busy);
        ctx.config.result_dir = not_a_dir.join("sub");
        assert!(matches!(
            Env::new(&ctx, Script::calm()),
            Err(BenchError::Io { .. })
        ));
    }

    /// Catches the CSV header written twice when a second run appends to the file, or not at all
    /// for a new one.
    #[tokio::test]
    async fn the_samples_file_has_one_header_however_many_runs_append() {
        let dir = temp("tb-engine-header");
        for attempt in 1..=2 {
            let env = env_attempt(dir.path(), Script::calm(), attempt);
            let a = arm("a", 1, 1, &Arc::new(Mutex::new(vec![])));
            run_cell(&env, CellId::G, vec![a], 2, None).await.unwrap();
        }
        let text = std::fs::read_to_string(dir.path().join("samples.csv")).unwrap();
        let headers = text.lines().filter(|l| l.starts_with("run_id,")).count();
        assert_eq!((headers, text.lines().count()), (1, 5));
    }

    /// Catches a key prefix that carries what an object key refuses, an empty run id, and a
    /// label that is not a label.
    #[test]
    fn the_prefix_and_the_slug_are_key_safe() {
        assert_eq!(slug("Run 7/x_y"), "run-7-x-y");
        let dir = temp("tb-engine-prefix");
        let mut ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        ctx.provenance.run_id = "Run 7/x".to_owned();
        let env = Env::new(&ctx, Script::calm()).unwrap();
        assert_eq!(env.prefix(), "tierstore-bench/run-7-x");
        ctx.provenance.run_id = String::new();
        let env = Env::new(&ctx, Script::calm()).unwrap();
        assert_eq!(env.prefix(), "tierstore-bench/unrecorded");
    }

    /// Catches a client made for a role the run does not configure.
    #[test]
    fn an_unconfigured_role_has_no_client() {
        let dir = temp("tb-engine-roles");
        let env = env(dir.path(), Window::Busy, Arc::new(ProcLoad));
        assert!(matches!(
            env.s3(Role::Cold, Cap::Cold),
            Err(BenchError::Config(_))
        ));
        assert!(matches!(
            env.stream(Role::Hot, Cap::Hot),
            Err(BenchError::Config(_))
        ));
    }

    /// Catches a step that does not carry what its request did.
    #[test]
    fn a_step_carries_its_outcome() {
        let outcome = Outcome {
            started_unix_ms: 7,
            latency: Duration::from_micros(250),
            status: Some(200),
            result: Ok(()),
        };
        let s = Step::of(Role::Instance, Op::Get, 99, &outcome)
            .writing(5)
            .labelled(".x");
        assert_eq!(
            (
                s.role,
                s.op,
                s.bytes,
                s.started_unix_ms,
                s.latency_us,
                s.status,
                s.ok
            ),
            (Role::Instance, Op::Get, 99, 7, 250, Some(200), true)
        );
        assert_eq!((s.cold_write, s.label, s.in_flight), (5, ".x", 0));
    }

    /// Catches a cleanup that does not run after the last pass, runs when the gate has cut the
    /// run (the objects of a cell must go whatever happened), has its notes lost, or has its
    /// failure swallowed.
    #[tokio::test]
    async fn the_cleanup_runs_once_at_the_end_whatever_ended_the_run() {
        let ran = Arc::new(AtomicUsize::new(0));
        let counting = |fail: bool| {
            let ran = ran.clone();
            hook(move |pass| {
                let ran = ran.clone();
                async move {
                    assert_eq!(pass, 0);
                    ran.fetch_add(1, Ordering::SeqCst);
                    if fail {
                        Err(BenchError::Request("cleanup"))
                    } else {
                        Ok(vec!["cleanup note".to_owned()])
                    }
                }
            })
        };
        let seen = Arc::new(Mutex::new(vec![]));
        let dir = temp("tb-engine-cleanup");
        let env = env(dir.path(), Window::Busy, Script::calm());
        let results = run_cell(
            &env,
            CellId::G,
            vec![arm("a", 1, 1, &seen)],
            2,
            Some(counting(false)),
        )
        .await
        .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(results[0].notes.contains(&"cleanup note".to_owned()));

        let env = env_attempt(dir.path(), Script::of(vec![Some(busy())]), 2);
        let err = run_cell(
            &env,
            CellId::G,
            vec![arm("a", 1, 1, &seen)],
            2,
            Some(counting(false)),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BenchError::Gate(_)));
        assert_eq!(ran.load(Ordering::SeqCst), 2);

        let env = env_attempt(dir.path(), Script::calm(), 3);
        let err = run_cell(
            &env,
            CellId::G,
            vec![arm("a", 1, 1, &seen)],
            2,
            Some(counting(true)),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BenchError::Request("cleanup")));

        let mut failing = arm("a", 1, 1, &seen);
        failing.work = work(|_| async { Err(BenchError::Request("work")) });
        let err = run_cell(&env, CellId::G, vec![failing], 2, Some(counting(true)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BenchError::Request("work")),
            "the first error is the one reported"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 4);
    }

    /// Catches the bytes an arm's setup writes once being left out of the admission of the cell
    /// (two arms of 150 each are over a cell cap of 250), or counted against a pass twice, and
    /// the handle a setup charges through not reaching the ledger.
    #[tokio::test]
    async fn the_setup_bytes_of_an_arm_are_part_of_what_a_cell_costs() {
        let dir = temp("tb-engine-once");
        let mut ctx = testkit::ctx(dir.path(), &[], Window::Busy);
        ctx.config.bytes = crate::config::ByteCaps {
            campaign: 1000,
            pass: 200,
            cell: 250,
        };
        let env = Env::new(&ctx, Script::calm()).unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let once = |label, bytes| {
            let mut a = arm(label, 1, 1, &seen);
            a.cold_bytes_once = bytes;
            a
        };
        let err = run_cell(
            &env,
            CellId::ColdRead,
            vec![once("a", 150), once("b", 150)],
            2,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("cell cap is 250 bytes and the cell needs 300"),
            "{err}"
        );
        let err = run_cell(&env, CellId::ColdRead, vec![once("a", 201)], 2, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pass cap is 200"), "{err}");
        run_cell(&env, CellId::ColdRead, vec![once("a", 150)], 2, None)
            .await
            .unwrap();
        env.charger().charge(150).unwrap();
        assert_eq!(env.spent(), 150);
    }

    /// Catches a streamed read counted good when it gave the wrong number of bytes, or when it
    /// failed, and a step that does not carry its read.
    #[test]
    fn a_streamed_read_is_good_only_if_it_gave_every_byte() {
        let read = |bytes, error| Streamed {
            started_unix_ms: 9,
            ttfb: Some(Duration::from_micros(5)),
            total: Duration::from_micros(70),
            bytes,
            status: Some(200),
            error,
        };
        let whole = Step::streamed(Role::Cold, Op::Get, 100, &read(100, None));
        assert!(whole.ok);
        assert_eq!(
            (whole.bytes, whole.latency_us, whole.started_unix_ms),
            (100, 70, 9)
        );
        assert!(!Step::streamed(Role::Cold, Op::Get, 100, &read(99, None)).ok);
        assert!(!Step::streamed(Role::Cold, Op::Get, 100, &read(100, Some("body"))).ok);
    }
}
