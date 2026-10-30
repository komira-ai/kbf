//! Cell cold-put, create-only puts straight to the cold store.
//!
//! What is built: the `direct` arm (this tool against the cold bucket named by the run
//! configuration, no instance in between), closed loop, healthy store. For each level in
//! `--in-flight` it sends `--puts` create-only puts (`If-None-Match: *`) of one object size
//! (`--object-bytes`, default 1 MiB) of seeded pseudo-random bytes, with that many requests in
//! flight, under the key prefix `<prefix>/<run id>/c<level>/<index>`. It records the time from
//! sending a put until the store answers, the status of every answer, and the rate. With
//! `--same-key` it instead runs the concurrent-create check: `--in-flight` writers (one value)
//! each put different bytes to ONE key, released together; it reports the status counts, which
//! writers were answered `200`, and whether the stored bytes are the winner's.
//!
//! What it holds to:
//! - Only the prefix. The run id and prefix are checked before anything is sent
//!   ([`run_prefix`]); a key is never built outside `<prefix>/<run id>/`. [`crate::cleanup`]
//!   deletes what the request log says was written there.
//! - Caps. A level ends when its planned puts are sent, when `--level-secs` have passed since it
//!   started, or when the bytes it would send exceed the smaller of the pass cap and what is
//!   left of the cell and campaign caps ([`crate::config::ByteCaps`]); it says which in its
//!   notes. A level whose budget cannot hold one object ends the run. Requests are paced to
//!   `--cold-rps` for the whole process.
//! - The load gate. The machine's load ([`crate::loadmeter`]) is read before each level and every
//!   `--gate-every` puts; a trip ends the level (its pass is `busy`, `gate_tripped` names the
//!   measure) and the run: later levels are not started. So does a stop file (`--stop-file`),
//!   which is looked for before every put, and a load that cannot be read.
//! - No warm-up pass. Each level is one measured pass, index 0, `discarded: false`; the figures
//!   are raw, in the notes (rates, p50, p90, p99, max over every request, whatever the window)
//!   and in the CSV of samples. `p99_us` of the result keeps the schema's rule (quiet window,
//!   1,000 requests at least).
//!
//! A `412` is an answer, counted as a `412`; a request with no answer (connect failure, timeout)
//! is counted as `none`. `failures` counts everything but `200` and `412`.
//!
//! Not built: the `instance` arm, the slow and stopped store (fault proxy), open loop.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::future::{join, join_all};
use tokio::sync::Barrier;
use tokio::time::{Instant as TokioInstant, sleep, sleep_until};

use crate::cells::{CellContext, CellId, CellPlan};
use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::gate::{LoadGate, LoadSample};
use crate::loadmeter::{LoadSource, ProcLoad};
use crate::output::{SampleWriter, write_result};
use crate::result::{
    CellResult, Op, PassResult, Percentiles, Provenance, SCHEMA_VERSION, Sample, Window,
};
use crate::role::Role;
use crate::s3::{BenchS3, Timeouts, unix_ms};

/// The object size when `--object-bytes` names none.
pub const DEFAULT_OBJECT_BYTES: u64 = 1 << 20;

/// The most bytes of bodies one level may have in flight (in flight x object size).
pub const MAX_IN_FLIGHT_BYTES: u64 = 1 << 30;

/// Requests a quiet window needs before `p99_us` is claimed.
const P99_MIN_REQUESTS: u64 = 1000;

/// What the `run` verb asks of this cell beyond the shared [`CellPlan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdPutPlan {
    /// Names the run; the second part of every key's prefix.
    pub run_id: String,
    /// The first part of every key's prefix.
    pub prefix: String,
    /// Puts per level.
    pub puts: u64,
    /// Seconds a level may take.
    pub level_secs: u64,
    /// Puts between two reads of the machine's load; 0 reads it only before a level.
    pub gate_every: u64,
    /// Run the concurrent-create check instead of the levels.
    pub same_key: bool,
    /// For the concurrent-create check: wait until this time (milliseconds since the Unix epoch)
    /// before releasing the writers, so several machines can start together.
    pub start_at_unix_ms: Option<u64>,
    /// A file whose presence ends the run before the next put.
    pub stop_file: Option<PathBuf>,
    /// Where the machine's load is read from (`/proc`; another directory for a test).
    pub proc_dir: PathBuf,
    /// Where the disks are listed (`/sys/block`; another directory for a test).
    pub sys_block_dir: PathBuf,
}

/// What the concurrent-create check saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SameKeyReport {
    /// Writers released.
    pub writers: u32,
    /// How many writers got each status (`None`: no answer).
    pub statuses: BTreeMap<Option<u16>, u32>,
    /// Indices of the writers answered `200`.
    pub winners: Vec<usize>,
    /// The stored object's length, if there is one.
    pub stored_len: Option<u64>,
    /// Indices of the writers whose bytes equal the stored object's.
    pub equal_to: Vec<usize>,
    /// With exactly one winner: whether the stored bytes are its bytes. `None` otherwise.
    pub stored_equals_winner: Option<bool>,
}

/// What a run of the cell produced.
#[derive(Debug)]
pub struct ColdPutRun {
    /// One result per level (one for the concurrent-create check), in the order run; the last
    /// is the level that was halted, if one was.
    pub results: Vec<CellResult>,
    /// Why the run was stopped before its last level, if it was.
    pub halted: Option<String>,
    /// The report of the concurrent-create check, when that was run.
    pub same_key: Option<SameKeyReport>,
}

/// The prefix `<prefix>/<run id>/` every key of a run is under.
///
/// # Errors
/// [`BenchError::Request`] if the run id is empty or has anything but letters, digits, `.`, `_`
/// and `-` (or starts with a dot or dash), or the prefix is empty or has an empty, `.` or `..`
/// segment or characters other than those and `/`.
pub fn run_prefix(prefix: &str, run_id: &str) -> Result<String, BenchError> {
    let plain = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    let id_ok = run_id.starts_with(|c: char| c.is_ascii_alphanumeric()) && plain(run_id);
    if !id_ok {
        return Err(BenchError::Request(
            "run id: letters, digits, '.', '_' and '-', starting with a letter or digit",
        ));
    }
    let prefix = prefix.trim_end_matches('/');
    let prefix_ok = !prefix.is_empty()
        && prefix
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && plain(seg));
    if !prefix_ok {
        return Err(BenchError::Request(
            "prefix: non-empty segments of letters, digits, '.', '_' and '-' joined by '/'",
        ));
    }
    Ok(format!("{prefix}/{run_id}/"))
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `len` pseudo-random bytes, the same for the same `(seed, stream, index)` and different for
/// another.
#[must_use]
pub fn body(seed: u64, stream: u32, index: u64, len: u64) -> Bytes {
    let mut state = seed ^ (u64::from(stream) << 40) ^ index.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let word = splitmix(&mut state).to_le_bytes();
        let take = (len - out.len()).min(word.len());
        out.extend_from_slice(&word[..take]);
    }
    Bytes::from(out)
}

/// The value at percentile `pct` of the ascending `sorted` (nearest rank); 0 if it is empty.
#[must_use]
pub fn nearest_rank(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (pct * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// `count` per second to three decimals, from microseconds, in integers.
fn per_second(count: u64, elapsed_us: u64) -> String {
    if elapsed_us == 0 {
        return "n/a".to_owned();
    }
    let milli = u128::from(count) * 1_000_000_000 / u128::from(elapsed_us);
    format!("{}.{:03}", milli / 1000, milli % 1000)
}

/// `200=7 412=1 none=2`: ascending statuses, then the requests with no answer.
fn status_counts(counts: &BTreeMap<Option<u16>, u32>) -> String {
    let mut parts: Vec<String> = counts
        .iter()
        .filter_map(|(status, n)| status.map(|s| format!("{s}={n}")))
        .collect();
    if let Some(n) = counts.get(&None) {
        parts.push(format!("none={n}"));
    }
    parts.join(" ")
}

/// The note (and warning) for a load gate pointed away from the host, `None` for the host's own.
fn foreign_load_note(proc_dir: &Path, sys_block_dir: &Path) -> Option<String> {
    if proc_dir == Path::new("/proc") && sys_block_dir == Path::new("/sys/block") {
        return None;
    }
    Some(format!(
        "load read from {} and {}, not the host's /proc and /sys/block: the load gate does not judge this machine",
        proc_dir.display(),
        sys_block_dir.display()
    ))
}

/// What share of a level's wall time the client spent making bodies, for a reader judging
/// whether the client bounded the rate. Hashing and signing happen inside the puts and are not
/// separated, so the figure is a floor of the client's own work.
fn client_note(generating_us: u64, elapsed_us: u64) -> String {
    let share = if elapsed_us == 0 {
        "n/a".to_owned()
    } else {
        format!(
            "{}%",
            u128::from(generating_us) * 100 / u128::from(elapsed_us)
        )
    };
    format!(
        "client work: bodies generated in {generating_us} us of {elapsed_us} us wall ({share}); all workers share one thread and hashing and signing are inside the puts, so this is a floor of the client's share"
    )
}

fn load_line(label: &str, sample: &LoadSample) -> String {
    format!(
        "load {label}: loadavg_1m={:.2} psi_cpu_avg10={:.2} psi_io_avg10={:.2} disk_util_pct={:.1}",
        sample.loadavg_1m, sample.psi_cpu_avg10, sample.psi_io_avg10, sample.disk_util_pct
    )
}

/// Spaces the starts of requests at no more than a rate, across all tasks.
struct Pacer {
    gap: Duration,
    next: Mutex<TokioInstant>,
}

impl Pacer {
    fn new(per_second: u32) -> Self {
        Self {
            gap: Duration::from_secs(1) / per_second.max(1),
            next: Mutex::new(TokioInstant::now()),
        }
    }

    async fn wait(&self) {
        let at = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            let at = (*next).max(TokioInstant::now());
            *next = at + self.gap;
            at
        };
        if at > TokioInstant::now() {
            sleep_until(at).await;
        }
    }
}

/// Why a level ended before its planned puts were sent.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Stop {
    TimeCap,
    ByteCap,
    FileFound,
    Gate(&'static str),
    LoadUnreadable,
}

impl Stop {
    fn name(&self) -> String {
        match self {
            Stop::TimeCap => "level-time-cap".to_owned(),
            Stop::ByteCap => "byte-cap".to_owned(),
            Stop::FileFound => "stop-file".to_owned(),
            Stop::Gate(metric) => format!("load gate: {metric}"),
            Stop::LoadUnreadable => "load figures unreadable".to_owned(),
        }
    }

    /// Whether the run, not only the level, ends.
    fn halts_run(&self) -> bool {
        !matches!(self, Stop::TimeCap | Stop::ByteCap)
    }

    /// The measure a load gate tripped on, if that is what this is.
    fn tripped_measure(&self) -> Option<String> {
        match self {
            Stop::Gate(metric) => Some((*metric).to_owned()),
            _ => None,
        }
    }
}

/// The checks every level or race makes before its first request.
fn start_check(
    plan: &ColdPutPlan,
    gate: &LoadGate,
    source: &dyn LoadSource,
) -> (Option<Stop>, Option<LoadSample>) {
    if plan.stop_file.as_ref().is_some_and(|f| f.exists()) {
        return (Some(Stop::FileFound), None);
    }
    match source.sample() {
        Ok(sample) => match gate.check(&sample) {
            Ok(()) => (None, Some(sample)),
            Err(trip) => (Some(Stop::Gate(trip.metric)), Some(sample)),
        },
        Err(_) => (Some(Stop::LoadUnreadable), None),
    }
}

/// Everything the tasks of one level share.
struct Level {
    s3: Arc<BenchS3>,
    arm: String,
    key_prefix: String,
    level: u32,
    size: u64,
    seed: u64,
    puts: u64,
    budget: u64,
    gate_every: u64,
    gate: LoadGate,
    source: Arc<dyn LoadSource>,
    deadline: Instant,
    stop_file: Option<PathBuf>,
    window: Window,
    pacer: Pacer,
    next: AtomicU64,
    attempted: AtomicU64,
    in_flight: AtomicU32,
    stopped: AtomicBool,
    stop: Mutex<Option<Stop>>,
    /// Microseconds the workers spent making bodies, all added.
    generating_us: AtomicU64,
}

impl Level {
    /// Ends the level for `why`; the first reason stays.
    fn end(&self, why: Stop) {
        let mut stop = self.stop.lock().unwrap_or_else(PoisonError::into_inner);
        if stop.is_none() {
            *stop = Some(why);
        }
        self.stopped.store(true, Ordering::Release);
    }

    fn check_gate(&self) {
        match self.source.sample() {
            Ok(sample) => {
                if let Err(trip) = self.gate.check(&sample) {
                    self.end(Stop::Gate(trip.metric));
                }
            }
            Err(_) => self.end(Stop::LoadUnreadable),
        }
    }
}

/// One of the tasks of a level: takes the next index until the level is over.
async fn worker(lv: Arc<Level>) -> Result<Vec<Sample>, BenchError> {
    let mut samples = Vec::new();
    loop {
        if lv.stopped.load(Ordering::Acquire) {
            break;
        }
        if lv.stop_file.as_ref().is_some_and(|f| f.exists()) {
            lv.end(Stop::FileFound);
            break;
        }
        if Instant::now() >= lv.deadline {
            lv.end(Stop::TimeCap);
            break;
        }
        let index = lv.next.fetch_add(1, Ordering::Relaxed);
        if index >= lv.puts {
            break;
        }
        if lv.gate_every > 0 && index > 0 && index.is_multiple_of(lv.gate_every) {
            lv.check_gate();
            if lv.stopped.load(Ordering::Acquire) {
                break;
            }
        }
        if lv.attempted.fetch_add(lv.size, Ordering::Relaxed) + lv.size > lv.budget {
            lv.end(Stop::ByteCap);
            break;
        }
        lv.pacer.wait().await;
        let made = Instant::now();
        let payload = body(lv.seed, lv.level, index, lv.size);
        let made_us = u64::try_from(made.elapsed().as_micros()).unwrap_or(u64::MAX);
        lv.generating_us.fetch_add(made_us, Ordering::Relaxed);
        let key = format!("{}{index:08}", lv.key_prefix);
        let in_flight = lv.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        let sent = lv.s3.put(&key, payload).await;
        lv.in_flight.fetch_sub(1, Ordering::Relaxed);
        let outcome = sent?;
        samples.push(Sample {
            cell: CellId::ColdPut,
            arm: lv.arm.clone(),
            pass: 0,
            window: lv.window,
            role: Role::Cold,
            op: Op::Put,
            bytes: lv.size,
            in_flight,
            started_unix_ms: outcome.started_unix_ms,
            latency_us: outcome.latency_us(),
            status: outcome.status,
        });
    }
    Ok(samples)
}

/// Counts of statuses of `samples`.
fn count_statuses(samples: &[Sample]) -> BTreeMap<Option<u16>, u32> {
    let mut counts = BTreeMap::new();
    for sample in samples {
        *counts.entry(sample.status).or_insert(0) += 1;
    }
    counts
}

/// The pass and the raw notes of the requests in `samples`.
fn summarize(
    samples: &[Sample],
    window: Window,
    gate_tripped: Option<String>,
    elapsed_us: u64,
) -> (PassResult, Vec<String>) {
    let requests = u64::try_from(samples.len()).unwrap_or(u64::MAX);
    let counts = count_statuses(samples);
    let failures: u64 = counts
        .iter()
        .filter(|(s, _)| !matches!(s, Some(200 | 412)))
        .map(|(_, n)| u64::from(*n))
        .sum();
    let ok = u64::from(counts.get(&Some(200)).copied().unwrap_or(0));
    // The headline figures are the latencies of the PUTs the store took (200). A 412 is a
    // lookup, and a request with no answer ends at a timeout; neither says how long a write takes.
    let mut latencies: Vec<u64> = samples
        .iter()
        .filter(|s| s.status == Some(200))
        .map(|s| s.latency_us)
        .collect();
    latencies.sort_unstable();
    let max_us = latencies.last().copied().unwrap_or(0);
    let other_max_us = samples
        .iter()
        .filter(|s| s.status != Some(200))
        .map(|s| s.latency_us)
        .max();
    let stored_bytes: u64 = samples
        .iter()
        .filter(|s| s.status == Some(200))
        .map(|s| s.bytes)
        .sum();
    let attempted_bytes: u64 = samples.iter().map(|s| s.bytes).sum();
    let window = if gate_tripped.is_some() {
        Window::Busy
    } else {
        window
    };
    let percentiles = (!latencies.is_empty()).then(|| Percentiles {
        p50_us: nearest_rank(&latencies, 50),
        p95_us: nearest_rank(&latencies, 95),
        p99_us: (window == Window::Quiet && ok >= P99_MIN_REQUESTS)
            .then(|| nearest_rank(&latencies, 99)),
        max_us,
    });
    let mut notes = Vec::new();
    if requests > 0 {
        notes.push(format!("status counts: {}", status_counts(&counts)));
        notes.push(format!(
            "bytes = stored by the 200 answers: {stored_bytes}; attempted by every request: {attempted_bytes}"
        ));
        if !latencies.is_empty() {
            notes.push(format!(
                "raw latency_us over the {ok} PUTs answered 200 only (the percentiles of this result), whatever the window: p50={} p90={} p99={} max={max_us}",
                nearest_rank(&latencies, 50),
                nearest_rank(&latencies, 90),
                nearest_rank(&latencies, 99),
            ));
        }
        if let Some(other) = other_max_us {
            notes.push(format!(
                "requests not answered 200 are left out of those percentiles; their slowest took latency_us={other}"
            ));
        }
        notes.push(format!(
            "raw rate: ok_put_per_s={} all_requests_per_s={} elapsed_us={elapsed_us}",
            per_second(ok, elapsed_us),
            per_second(requests, elapsed_us),
        ));
    }
    let pass = PassResult {
        pass: 0,
        window,
        discarded: false,
        requests,
        failures,
        bytes: stored_bytes,
        elapsed_us,
        percentiles,
        gate_tripped,
    };
    (pass, notes)
}

/// With exactly one winner, whether the stored bytes are that writer's; `None` with none or several.
fn winner_check(winners: &[usize], equal_to: &[usize]) -> Option<bool> {
    match winners {
        [winner] => Some(equal_to.contains(winner)),
        _ => None,
    }
}

/// The words for [`winner_check`]'s answer in the notes.
fn winner_text(check: Option<bool>, winners: usize) -> String {
    match check {
        Some(true) => "yes".to_owned(),
        Some(false) => "no".to_owned(),
        None => format!("n/a ({winners} winners here)"),
    }
}

/// What the concurrent-create check waits on before it releases its writers at a given time.
pub trait ReleaseWait {
    /// Returns when the wall clock reads `at` (milliseconds since the Unix epoch), or at once if
    /// it already does.
    fn until(&self, at: u64) -> impl Future<Output = ()>;
}

/// The real thing: sleeps until the wall clock reaches the time.
pub struct WallClock;

impl ReleaseWait for WallClock {
    async fn until(&self, at: u64) {
        sleep(Duration::from_millis(at.saturating_sub(unix_ms()))).await;
    }
}

/// Shared pieces of the run, built once.
struct Run<'a, W> {
    ctx: &'a CellContext,
    plan: &'a CellPlan,
    s3: Arc<BenchS3>,
    source: Arc<dyn LoadSource>,
    wait: W,
    /// Set when the load is read from somewhere other than the host's `/proc` and `/sys/block`.
    load_note: Option<String>,
    provenance: Provenance,
    samples_out: SampleWriter,
    under: String,
    size: u64,
}

impl<W: ReleaseWait> Run<'_, W> {
    fn result(
        &self,
        arm: String,
        started_unix_ms: u64,
        pass: PassResult,
        mut notes: Vec<String>,
    ) -> CellResult {
        notes.extend(self.load_note.clone());
        CellResult {
            schema_version: SCHEMA_VERSION,
            cell: CellId::ColdPut,
            arm,
            started_unix_ms,
            finished_unix_ms: unix_ms(),
            provenance: self.provenance.clone(),
            passes: vec![pass],
            notes,
        }
    }

    /// Writes the result file and prints the summary lines.
    fn finish(&self, result: CellResult) -> Result<CellResult, BenchError> {
        write_result(&self.ctx.config.result_dir, &result)?;
        println!("{}: {} requests", result.arm, result.passes[0].requests);
        for note in &result.notes {
            println!("{}: {note}", result.arm);
        }
        Ok(result)
    }

    /// The result of a level or race that did not start, and why.
    fn not_started(
        &self,
        arm: String,
        started: u64,
        why: Stop,
        mut notes: Vec<String>,
    ) -> Result<(CellResult, Stop), BenchError> {
        notes.push(format!("ended by: {}, before any request", why.name()));
        let (pass, _) = summarize(&[], self.ctx.config.window, why.tripped_measure(), 0);
        let result = self.finish(self.result(arm, started, pass, notes))?;
        Ok((result, why))
    }

    /// One level. Returns its result and why the run must stop, if it must.
    async fn level(
        &mut self,
        level: u32,
        used: &mut u64,
    ) -> Result<(CellResult, Option<Stop>), BenchError> {
        let (ctx, plan) = (self.ctx, self.plan);
        let (config, cp) = (&ctx.config, &plan.cold_put);
        let arm = format!("direct-c{level}");
        let started = unix_ms();
        let mut notes = vec![
            format!(
                "level: {level} in flight, {} planned puts of {} bytes, run window {}",
                cp.puts,
                self.size,
                config.window.as_str()
            ),
            "no warm-up pass: this level is one measured pass, reported raw with no verdict"
                .to_owned(),
        ];
        let budget = config
            .bytes
            .pass
            .min(config.bytes.cell.saturating_sub(*used))
            .min(config.bytes.campaign.saturating_sub(*used));
        let (mut stop, start_sample) = start_check(cp, &config.gate, self.source.as_ref());
        if let Some(sample) = &start_sample {
            notes.push(load_line("at start", sample));
        }
        if stop.is_none() && budget < self.size {
            stop = Some(Stop::ByteCap);
            notes.push("the byte budget left for this level holds less than one object".to_owned());
        }
        if let Some(why) = stop {
            let (result, why) = self.not_started(arm, started, why, notes)?;
            return Ok((result, Some(why)));
        }

        let deadline = Instant::now()
            .checked_add(Duration::from_secs(cp.level_secs))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 3600));
        let lv = Arc::new(Level {
            s3: self.s3.clone(),
            arm: arm.clone(),
            key_prefix: format!("{}c{level}/", self.under),
            level,
            size: self.size,
            seed: config.seed,
            puts: cp.puts,
            budget,
            gate_every: cp.gate_every,
            gate: config.gate,
            source: self.source.clone(),
            deadline,
            stop_file: cp.stop_file.clone(),
            window: config.window,
            pacer: Pacer::new(config.rates.cold_rps),
            next: AtomicU64::new(0),
            attempted: AtomicU64::new(0),
            in_flight: AtomicU32::new(0),
            stopped: AtomicBool::new(false),
            stop: Mutex::new(None),
            generating_us: AtomicU64::new(0),
        });
        let clock = Instant::now();
        let mut samples = Vec::new();
        for done in join_all((0..level).map(|_| worker(lv.clone()))).await {
            samples.extend(done?);
        }
        let elapsed_us = u64::try_from(clock.elapsed().as_micros()).unwrap_or(u64::MAX);
        samples.sort_by_key(|s| s.started_unix_ms);
        *used += samples.iter().map(|s| s.bytes).sum::<u64>();

        let stop = lv
            .stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let gate_tripped = stop.as_ref().and_then(Stop::tripped_measure);
        if gate_tripped.is_some() {
            for sample in &mut samples {
                sample.window = Window::Busy;
            }
        }
        self.samples_out.append(&self.provenance, &samples)?;
        let (pass, summary) = summarize(&samples, config.window, gate_tripped, elapsed_us);
        notes.extend(summary);
        notes.push(client_note(
            lv.generating_us.load(Ordering::Relaxed),
            elapsed_us,
        ));
        notes.push(format!(
            "ended by: {}",
            stop.as_ref()
                .map_or_else(|| "planned count".to_owned(), Stop::name)
        ));
        if let Ok(sample) = self.source.sample() {
            notes.push(load_line("at end", &sample));
        }
        let result = self.finish(self.result(arm, started, pass, notes))?;
        Ok((result, stop.filter(Stop::halts_run)))
    }

    /// The concurrent-create check: `writers` puts of different bytes to one key, together.
    async fn same_key(
        &mut self,
        writers: u32,
    ) -> Result<(CellResult, Option<SameKeyReport>, Option<Stop>), BenchError> {
        let (ctx, plan) = (self.ctx, self.plan);
        let (config, cp) = (&ctx.config, &plan.cold_put);
        let arm = format!("direct-same-key-c{writers}");
        let started = unix_ms();
        let mut notes = vec![format!(
            "same-key: {writers} concurrent create-only puts of {} bytes to one key, run window {}",
            self.size,
            config.window.as_str()
        )];
        let (mut stop, start_sample) = start_check(cp, &config.gate, self.source.as_ref());
        if let Some(sample) = &start_sample {
            notes.push(load_line("at start", sample));
        }
        let room = config
            .bytes
            .pass
            .min(config.bytes.cell)
            .min(config.bytes.campaign);
        if stop.is_none() && room < u64::from(writers) * self.size {
            stop = Some(Stop::ByteCap);
            notes.push("the byte budget holds less than one object per writer".to_owned());
        }
        if let Some(why) = stop {
            let (result, why) = self.not_started(arm, started, why, notes)?;
            return Ok((result, None, Some(why)));
        }

        let key = format!("{}same-key", self.under);
        let bodies: Vec<Bytes> = (0..u64::from(writers))
            .map(|i| body(config.seed, writers, i, self.size))
            .collect();
        // The writers wait at a barrier that the starter joins last, after `start_at_unix_ms`.
        let release = Barrier::new(usize::try_from(writers).unwrap_or(usize::MAX) + 1);
        let (s3, size, window) = (&self.s3, self.size, config.window);
        let put_all = join_all(bodies.iter().cloned().enumerate().map(|(i, payload)| {
            let (release, key, arm) = (&release, &key, &arm);
            async move {
                release.wait().await;
                let outcome = s3.put(key, payload).await?;
                Ok::<_, BenchError>((
                    i,
                    Sample {
                        cell: CellId::ColdPut,
                        arm: arm.clone(),
                        pass: 0,
                        window,
                        role: Role::Cold,
                        op: Op::Put,
                        bytes: size,
                        in_flight: writers,
                        started_unix_ms: outcome.started_unix_ms,
                        latency_us: outcome.latency_us(),
                        status: outcome.status,
                    },
                ))
            }
        }));
        let start_at = cp.start_at_unix_ms;
        let wait = &self.wait;
        let starter = async {
            if let Some(at) = start_at {
                wait.until(at).await;
            }
            let released = Instant::now();
            release.wait().await;
            released
        };
        let (released, put_results) = join(starter, put_all).await;
        let elapsed_us = u64::try_from(released.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut answered: Vec<(usize, Sample)> = Vec::new();
        for done in put_results {
            answered.push(done?);
        }
        answered.sort_by_key(|(i, _)| *i);
        let samples: Vec<Sample> = answered.iter().map(|(_, s)| s.clone()).collect();
        self.samples_out.append(&self.provenance, &samples)?;

        let winners: Vec<usize> = answered
            .iter()
            .filter(|(_, s)| s.status == Some(200))
            .map(|(i, _)| *i)
            .collect();
        let stored_len = self.s3.head(&key).await?.result.ok().flatten();
        let stored = match stored_len.filter(|len| *len > 0) {
            Some(len) => self.s3.get_range(&key, 0, len).await?.result.ok(),
            None => None,
        };
        let equal_to: Vec<usize> = stored.as_ref().map_or_else(Vec::new, |bytes| {
            bodies
                .iter()
                .enumerate()
                .filter(|(_, b)| *b == bytes)
                .map(|(i, _)| i)
                .collect()
        });
        let stored_equals_winner = winner_check(&winners, &equal_to);
        let report = SameKeyReport {
            writers,
            statuses: count_statuses(&samples),
            winners: winners.clone(),
            stored_len,
            equal_to: equal_to.clone(),
            stored_equals_winner,
        };

        let (pass, summary) = summarize(&samples, config.window, None, elapsed_us);
        notes.extend(summary);
        notes.push(format!("winners (answered 200): {}", winners.len()));
        notes.push(format!(
            "stored length (HEAD after the race): {}",
            stored_len.map_or_else(|| "none".to_owned(), |l| l.to_string())
        ));
        notes.push(format!(
            "stored bytes equal the winner's: {}",
            winner_text(stored_equals_winner, winners.len())
        ));
        notes.push(format!(
            "stored bytes equal the bytes of this process's writers: {equal_to:?}"
        ));
        let result = self.finish(self.result(arm, started, pass, notes))?;
        Ok((result, Some(report), None))
    }
}

/// Runs the cell with `source` as the machine's load. Result files are written as each level
/// ends, so a run that is halted leaves the levels it did.
///
/// # Errors
/// [`BenchError`]: the plan is not valid (run id, prefix, sizes, levels), no cold store is
/// configured, a file cannot be written, or the request log cannot be written. A halted run is
/// not an error here: see [`ColdPutRun::halted`].
pub async fn run_levels(
    ctx: &CellContext,
    plan: &CellPlan,
    source: impl LoadSource + 'static,
) -> Result<ColdPutRun, BenchError> {
    run_levels_with(ctx, plan, source, WallClock).await
}

/// [`run_levels`] with the wait for a `--start-at-unix-ms` time given, so a test can see when
/// the writers are released without waiting for a wall clock.
///
/// # Errors
/// As [`run_levels`].
pub async fn run_levels_with(
    ctx: &CellContext,
    plan: &CellPlan,
    source: impl LoadSource + 'static,
    wait: impl ReleaseWait,
) -> Result<ColdPutRun, BenchError> {
    let cp = &plan.cold_put;
    let under = run_prefix(&cp.prefix, &cp.run_id)?;
    let size = match plan.object_bytes.as_slice() {
        [] => DEFAULT_OBJECT_BYTES,
        [one] if *one > 0 => *one,
        _ => {
            return Err(BenchError::Request(
                "cold-put takes one object size of at least one byte",
            ));
        }
    };
    if plan.in_flight.is_empty() || plan.in_flight.contains(&0) {
        return Err(BenchError::Request(
            "requests in flight: at least one level, each at least 1",
        ));
    }
    let widest = u64::from(plan.in_flight.iter().copied().max().unwrap_or(0));
    if widest.saturating_mul(size) > MAX_IN_FLIGHT_BYTES {
        return Err(BenchError::Request(
            "in flight x object bytes is over 1 GiB: the bodies of one level are held in memory",
        ));
    }
    if cp.same_key && plan.in_flight.len() != 1 {
        return Err(BenchError::Request(
            "same-key takes one count of concurrent writers",
        ));
    }
    let spec: &StoreSpec = ctx.config.store(Role::Cold)?;
    let s3 = Arc::new(BenchS3::new(
        Role::Cold,
        spec,
        Timeouts::default(),
        ctx.log.clone(),
    )?);
    let mut provenance = ctx.provenance.clone();
    if provenance.run_id.is_empty() {
        provenance.run_id.clone_from(&cp.run_id);
    }
    provenance.tool_versions.insert(
        "tierstore-bench".to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    let load_note = foreign_load_note(&cp.proc_dir, &cp.sys_block_dir);
    if let Some(note) = &load_note {
        eprintln!("tierstore-bench: warning: {note}");
    }
    let mut run = Run {
        ctx,
        plan,
        s3,
        source: Arc::new(source),
        wait,
        load_note,
        provenance,
        samples_out: SampleWriter::open(&ctx.config.result_dir)?,
        under,
        size,
    };

    let mut out = ColdPutRun {
        results: Vec::new(),
        halted: None,
        same_key: None,
    };
    if cp.same_key {
        let (result, report, stop) = run.same_key(plan.in_flight[0]).await?;
        out.results.push(result);
        out.same_key = report;
        out.halted = stop.map(|s| s.name());
        return Ok(out);
    }
    let mut used = 0;
    for &level in &plan.in_flight {
        let (result, stop) = run.level(level, &mut used).await?;
        out.results.push(result);
        if let Some(why) = stop {
            out.halted = Some(why.name());
            break;
        }
    }
    Ok(out)
}

/// Runs the cell against the real machine's load. A halted run is an error
/// ([`BenchError::Halted`]) after its result files are written.
///
/// # Errors
/// What [`run_levels`] returns, and [`BenchError::Halted`].
pub async fn run(ctx: &CellContext, plan: &CellPlan) -> Result<Vec<CellResult>, BenchError> {
    let load = ProcLoad::at(
        plan.cold_put.proc_dir.clone(),
        plan.cold_put.sys_block_dir.clone(),
    );
    let run = run_levels(ctx, plan, load).await?;
    match run.halted {
        Some(why) => Err(BenchError::Halted(why)),
        None => Ok(run.results),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a body that is not reproducible from its seed (a rerun would write other bytes),
    /// two objects with the same bytes, or the wrong length when it is not a multiple of eight.
    #[test]
    fn a_body_is_fixed_by_its_seed_and_differs_by_stream_and_index() {
        let a = body(7, 4, 0, 1001);
        assert_eq!(a.len(), 1001);
        assert_eq!(a, body(7, 4, 0, 1001));
        assert_ne!(a, body(8, 4, 0, 1001));
        assert_ne!(a, body(7, 5, 0, 1001));
        assert_ne!(a, body(7, 4, 1, 1001));
        assert_eq!(body(7, 4, 0, 0).len(), 0);
        assert!(a.iter().any(|b| *b != 0));
    }

    /// Catches an off-by-one in the nearest-rank percentile: of 1..=100, p50 is 50, p99 is 99,
    /// and a single value is every percentile.
    #[test]
    fn nearest_rank_picks_the_value_at_or_above_the_fraction() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&v, 50), 50);
        assert_eq!(nearest_rank(&v, 90), 90);
        assert_eq!(nearest_rank(&v, 99), 99);
        assert_eq!(nearest_rank(&v, 100), 100);
        assert_eq!(nearest_rank(&[42], 99), 42);
        assert_eq!(nearest_rank(&[], 50), 0);
        assert_eq!(nearest_rank(&[1, 2, 3], 50), 2);
    }

    /// Catches a rate printed with the wrong scale or a division by zero.
    #[test]
    fn rates_are_exact_to_three_decimals() {
        assert_eq!(per_second(10, 1_000_000), "10.000");
        assert_eq!(per_second(1, 3_000_000), "0.333");
        assert_eq!(per_second(5, 0), "n/a");
    }

    /// Catches the status counts losing the requests with no answer or sorting them among the
    /// statuses.
    #[test]
    fn status_counts_list_statuses_then_no_answer() {
        let counts = BTreeMap::from([(None, 2), (Some(412), 1), (Some(200), 7)]);
        assert_eq!(status_counts(&counts), "200=7 412=1 none=2");
    }

    /// Catches the run id and prefix rules letting a key out of its prefix.
    #[test]
    fn a_prefix_is_the_prefix_slash_run_id_slash() {
        assert_eq!(run_prefix("bench/c1", "r-1.a").unwrap(), "bench/c1/r-1.a/");
        assert_eq!(run_prefix("bench/c1/", "r1").unwrap(), "bench/c1/r1/");
        for (prefix, run_id) in [
            ("bench", ""),
            ("bench", ".x"),
            ("bench", "-x"),
            ("bench", "a/b"),
            ("", "r"),
            ("/", "r"),
            ("a//b", "r"),
            ("a/./b", "r"),
            ("a/../b", "r"),
            ("a b", "r"),
        ] {
            assert!(run_prefix(prefix, run_id).is_err(), "{prefix:?} {run_id:?}");
        }
    }

    /// Catches the rate cap not spacing requests: after a first request, the next two are due
    /// a gap apart each, so the pacer's due time moves forward by two gaps and the waits end.
    #[tokio::test]
    async fn the_pacer_spaces_request_starts_by_its_gap() {
        let pacer = Pacer::new(1000);
        assert_eq!(pacer.gap, Duration::from_millis(1));
        let first = *pacer.next.lock().unwrap();
        pacer.wait().await;
        pacer.wait().await;
        pacer.wait().await;
        let after = *pacer.next.lock().unwrap();
        assert!(
            after - first >= Duration::from_millis(3),
            "{:?}",
            after - first
        );
        // A rate of zero is read as one per second, not as a division by zero.
        assert_eq!(Pacer::new(0).gap, Duration::from_secs(1));
    }

    /// Catches the three answers about the stored bytes being mixed up: a single winner whose
    /// bytes are stored, a single winner whose bytes are not (the failure the check exists to
    /// find), and no winner or several (no answer).
    #[test]
    fn the_stored_bytes_are_compared_only_with_a_single_winner() {
        assert_eq!(winner_check(&[3], &[3]), Some(true));
        assert_eq!(winner_check(&[3], &[5]), Some(false));
        assert_eq!(winner_check(&[3], &[]), Some(false));
        assert_eq!(winner_check(&[], &[1]), None);
        assert_eq!(winner_check(&[1, 2], &[1]), None);
        assert_eq!(winner_text(Some(true), 1), "yes");
        assert_eq!(winner_text(Some(false), 1), "no");
        assert_eq!(winner_text(None, 2), "n/a (2 winners here)");
    }

    /// Catches a load gate pointed at a made-up tree going unmarked, and the host's own being
    /// marked: either directory away from the host's is named in the note, both at home is none.
    #[test]
    fn a_load_read_away_from_the_host_is_noted_and_the_hosts_own_is_not() {
        let host = (Path::new("/proc"), Path::new("/sys/block"));
        assert_eq!(foreign_load_note(host.0, host.1), None);
        let proc_away = foreign_load_note(Path::new("/x/proc"), host.1).unwrap();
        assert!(proc_away.contains("/x/proc and /sys/block"), "{proc_away}");
        assert!(proc_away.contains("not the host's"), "{proc_away}");
        let sys_away = foreign_load_note(host.0, Path::new("/x/sys")).unwrap();
        assert!(sys_away.contains("/proc and /x/sys"), "{sys_away}");
    }

    /// Catches the client's share of a level computed wrongly or dividing by zero: 250 us of
    /// 1,000 us is 25%, more than the wall time (a rounding of the clock) is over 100%, and a
    /// level with no wall time has no share.
    #[test]
    fn the_clients_share_of_a_level_is_its_body_time_over_the_wall_time() {
        assert!(client_note(250, 1000).contains("generated in 250 us of 1000 us wall (25%)"));
        assert!(client_note(1000, 1000).contains("(100%)"));
        assert!(client_note(0, 1000).contains("(0%)"));
        assert!(client_note(5, 0).contains("(n/a)"));
        assert!(client_note(1, 1).contains("floor of the client's share"));
    }

    /// Catches a stop's name or its reach changing: which endings stop the whole run and which
    /// only the level, and the measure a gate trip names.
    #[test]
    fn a_stop_names_itself_and_says_whether_it_ends_the_run() {
        let cases = [
            (Stop::TimeCap, "level-time-cap", false),
            (Stop::ByteCap, "byte-cap", false),
            (Stop::FileFound, "stop-file", true),
            (Stop::Gate("psi_io_avg10"), "load gate: psi_io_avg10", true),
            (Stop::LoadUnreadable, "load figures unreadable", true),
        ];
        for (stop, name, ends_run) in cases {
            assert_eq!(stop.name(), name);
            assert_eq!(stop.halts_run(), ends_run, "{name}");
        }
        assert_eq!(
            Stop::Gate("loadavg_1m").tripped_measure().as_deref(),
            Some("loadavg_1m")
        );
        assert_eq!(Stop::TimeCap.tripped_measure(), None);
    }
}
