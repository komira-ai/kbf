//! Learned sizes, in shadow (`--learned-sizes=shadow`): the farm shows `kbf-estimator`
//! every attempt that finishes and says, for every new operation, what the estimator
//! would book. Nothing it learns changes a booking: `off` (the default) calls the
//! estimator never, and `shadow` books what `off` books.
//!
//! # Where it listens
//!
//! On the result path, in the order results commit, for every attempt the scheduler took
//! ([`crate::farm`]'s report): not at submit, and not in the recorder, which sees only
//! recorded calls. An attempt is a sample only if its run carries a usage block that
//! names its instrument (`basis`); a run with none, or whose daemon predates the tag, is
//! counted as skipped and teaches nothing. Only `LeaseKind::Action` leases with no GPU
//! are shown, and only when the request's `needs` pin the OS (the key's platform class
//! would otherwise be a guess). An own-limit kill that carries no usage still counts: its
//! basis is its node's OS and its peak the limit it died at. An operation whose subject
//! was lost (restored from a row written without one) has no key.
//!
//! # The buckets
//!
//! Before an attempt is observed, the estimate for its key is asked for. The attempt then
//! falls into one of four buckets, counted as `kbf_estimator_shadow_total{level,bucket}`
//! (`level` is the summary level the estimate came from, `none` for `cold`):
//!
//! * `cold`: no estimate (too few samples, stale, unknown key).
//! * `would_oom`: the attempt's peak is above `memory_max(estimate)`, the limit a
//!   booking of the estimate would run under. A killed attempt counts as having used
//!   at least the limit it died at. This holds even when the peak is truncated.
//! * `censored`: the peak is only a lower bound (the attempt ran at 95% or more of its
//!   own limit, or was killed at it), or it came within 5% of the estimate's limit, so
//!   it would have run at that limit: no claim either way.
//! * `covered`: every other attempt, with its peak at most 95% of the estimate's limit.
//!
//! An attempt the estimator does not take as a memory sample (a failure that is not a
//! memory kill, a lost run, a run without a measured peak or under a second) is in no
//! bucket. The estimate is the estimator's own, which may be below today's default: the
//! buckets measure what a lowering would do, not what phase 1 books.
//!
//! # State
//!
//! The summaries live in memory and in the state store, one `est/` row per key (a version
//! byte, then the estimator's own checksummed row). They are never part of a requeue's
//! transaction: an error of the estimator's rows must not change what the scheduler
//! does. A summary an own-limit kill changed is written right after the requeue (or the
//! final failure) commits, on a best-effort basis: a refused write is logged, never
//! retried into the requeue, and the key is written again with its newest summary by the
//! write-behind; only a crash in that window loses the kill's floor. The rest is written
//! behind: dirty keys are batched every few seconds, and an hourly sweep, a few hundred
//! rows at a tick, drops the rows (and the samples) the estimator calls stale. Time is the farm's: the day of a result is the wall time the
//! farm was built at plus its [`FarmTime`], never a clock read here, and it never runs
//! backward.
//!
//! `off` does not read the rows, write them or delete them. A store keeps what `shadow`
//! wrote, untouched, and a later `shadow` takes it up again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use kbf_estimator::{
    Basis, Bounds, Cfg, Day, Estimate, Fallback, Hint, Instance, Key, Level, Outcome as RunOutcome,
    PlatformClass, Run, Shape, Summaries, Summary, memory_max,
};
use kbf_front::metrics::{ShadowBucket, ShadowLevel, ShadowTotals};
use kbf_invocation::{BookSource, Evidence, RunUsage, UsageBasis};
use kbf_types::{Failure, FarmTime, LeaseId, LeaseKind, Outcome, StartLease};

use kbf_front::Described;

use crate::action_usage::{ShapeKey, Subject};
use crate::machine::Platform;

mod predict;
use predict::Predictions;
pub use predict::{Axis, Fit, Forecast, Unset};

/// What `--learned-sizes` does with the estimator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LearnedSizes {
    /// The estimator is never called and nothing is read from or written to the store.
    #[default]
    Off,
    /// The estimator hears of every finished attempt and is asked for every new
    /// operation; the answers are logged and counted. Bookings are unchanged.
    Shadow,
    /// `shadow`, and one change to a booking: a kill at an action's own memory limit leaves
    /// a floor under the action's key that its later digests book at least, until the
    /// estimator's decay lowers it.
    On,
}

const DAY_MS: u64 = 86_400_000;
/// Rows written at once.
pub(crate) const BATCH: usize = 256;
/// How long dirty keys wait before they are written.
const FLUSH_EVERY_MS: u64 = 5_000;
/// How often the stale rows are swept.
const SWEEP_EVERY_MS: u64 = 3_600_000;
/// Leases whose booking is remembered; the oldest go first.
const STARTED_CAP: usize = 65_536;

/// One attempt, as the estimator takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The key of the action on the node's instrument.
    pub key: Key,
    /// How the run went.
    pub run: Run,
}

/// What the farm recorded when it started a lease.
#[derive(Clone, Copy, Debug)]
struct Started {
    booked: u64,
    eligible: bool,
}

/// Why an attempt or an operation is not shown to the estimator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skip {
    /// Not an action lease, or it asked for a GPU.
    Kind,
    /// The request's `needs` do not pin the OS.
    NoOs,
    /// The farm did not see the lease start (a lease taken over from a predecessor).
    NoBooking,
    /// The run carried no usage, or its daemon did not say which instrument measured it.
    NoBasis,
    /// The operation's subject is unknown (restored from a row that kept none), so every
    /// such action would share one key.
    NoSubject,
}

/// A summary's row, as the state store keeps it.
pub type Row = (Key, Vec<u8>);

/// The learned-sizes state of a farm.
#[derive(Debug)]
pub struct Learned {
    mode: LearnedSizes,
    summaries: Summaries,
    default_memory: u64,
    epoch_unix_ms: u64,
    now: FarmTime,
    started: BTreeMap<LeaseId, Started>,
    staged: BTreeMap<LeaseId, Attempt>,
    dirty: BTreeSet<Key>,
    tally: [[u64; 4]; 3],
    skipped: BTreeMap<Skip, u64>,
    calls: u64,
    flushed: FarmTime,
    swept: Option<FarmTime>,
    /// A sweep of the store that has more to do at the next tick, for this day.
    sweeping: Option<Day>,
    /// The size keys of the keyed operations seen under each estimator key (level 2): the
    /// key floors a decay of that key's summary lowers, whichever attempt causes it
    /// ([`Learned::register`]).
    named: BTreeMap<Key, BTreeSet<u64>>,
    /// The estimator key (level 2) of each size key in `named`, for the row of its floor.
    names: BTreeMap<u64, Key>,
    /// Key floors the estimator's decay has lowered since they were last taken: the size
    /// key, the floor the summary held before and the one it holds now.
    lowered: Vec<(u64, u64, u64)>,
    /// The predictions made for new operations, and what became of them
    /// ([`Learned::predict`]).
    predictions: Predictions,
}

impl Learned {
    /// The state of a farm built at `epoch_unix_ms` (wall time) whose first rung is
    /// `default_memory` bytes.
    #[must_use]
    pub fn new(mode: LearnedSizes, default_memory: u64, epoch_unix_ms: u64) -> Self {
        Self {
            mode,
            summaries: Summaries::default(),
            default_memory,
            epoch_unix_ms,
            now: FarmTime::default(),
            started: BTreeMap::new(),
            staged: BTreeMap::new(),
            dirty: BTreeSet::new(),
            tally: [[0; 4]; 3],
            skipped: BTreeMap::new(),
            calls: 0,
            flushed: FarmTime::default(),
            swept: None,
            sweeping: None,
            named: BTreeMap::new(),
            names: BTreeMap::new(),
            lowered: Vec::new(),
            predictions: Predictions::default(),
        }
    }

    /// Whether the estimator is off.
    #[must_use]
    pub fn off(&self) -> bool {
        self.mode == LearnedSizes::Off
    }

    /// Whether requests are keyed for the kill-driven floor (`on`).
    #[must_use]
    pub fn keys(&self) -> bool {
        self.mode == LearnedSizes::On
    }

    /// How many times the estimator was asked or shown a run. Always 0 while off.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// The summaries held, for a test.
    #[must_use]
    pub fn summaries(&self) -> &Summaries {
        &self.summaries
    }

    /// What the farm has not shown the estimator, by why.
    #[must_use]
    pub fn skipped(&self, why: Skip) -> u64 {
        self.skipped.get(&why).copied().unwrap_or(0)
    }

    /// The memory an action books when it has no estimate.
    #[must_use]
    pub fn default_memory(&self) -> u64 {
        self.default_memory
    }

    /// Counts something shown to the estimator never.
    pub fn skip(&mut self, why: Skip) {
        *self.skipped.entry(why).or_default() += 1;
    }

    /// Moves farm time on; it never moves back.
    pub fn clock(&mut self, now: FarmTime) {
        self.now = self.now.max(now);
    }

    /// The day of farm time `now`, on the wall clock the farm was built on.
    pub(crate) fn day(&self) -> Day {
        let ms = self.epoch_unix_ms.saturating_add(self.now.as_millis());
        Day(u32::try_from(ms / DAY_MS).unwrap_or(u32::MAX))
    }

    /// Takes up the rows the state store held when the farm started. A row the
    /// estimator cannot decode leaves its key cold. Does nothing while off.
    pub fn restore(&mut self, rows: &[Row]) {
        if self.off() {
            return;
        }
        for (key, bytes) in rows {
            if self.summaries.insert_encoded(key, bytes).is_err() {
                tracing::warn!("a stored size summary cannot be read: its action starts cold");
            }
        }
    }

    /// Remembers what a lease the farm starts books.
    pub fn on_start(&mut self, start: &StartLease) {
        if self.off() {
            return;
        }
        if self.started.len() >= STARTED_CAP {
            self.started.pop_first();
        }
        let eligible = start.kind == LeaseKind::Action && start.resources.gpus == 0;
        self.started.insert(
            start.lease,
            Started {
                booked: start.resources.memory_bytes,
                eligible,
            },
        );
    }

    /// What `lease` booked, if the farm saw it start and it is one the estimator is
    /// shown. Counts why not.
    pub fn booking(&mut self, lease: LeaseId) -> Result<u64, Skip> {
        let found = match self.started.get(&lease) {
            None => Err(Skip::NoBooking),
            Some(s) if !s.eligible => Err(Skip::Kind),
            Some(s) => Ok(s.booked),
        };
        if let Err(why) = found {
            self.skip(why);
        }
        found
    }

    /// A lease the scheduler gave up or finished: its booking is no longer needed.
    pub fn forget(&mut self, lease: LeaseId) {
        self.started.remove(&lease);
    }

    /// Records that a keyed operation with size key `size_key` was submitted for the
    /// estimator key `key` (any level): when the decay lowers that key's floor, the floor
    /// under `size_key` is reported, whichever attempt of the key causes the decay (one the
    /// client sized, which is never keyed, included). Only with `on`.
    pub fn register(&mut self, key: &Key, size_key: u64) {
        if !self.keys() {
            return;
        }
        if self.named.len() >= STARTED_CAP
            && let Some((_, gone)) = self.named.pop_first()
        {
            self.names.retain(|size_key, _| !gone.contains(size_key));
        }
        self.named.entry(key.l2()).or_default().insert(size_key);
        self.names.insert(size_key, key.l2());
    }

    /// The estimator key (level 2) the keyed operations with `size_key` were registered
    /// under, if any.
    #[must_use]
    pub fn summary_key(&self, size_key: u64) -> Option<Key> {
        self.names.get(&size_key).cloned()
    }

    /// The size keys registered under the estimator key `key`, for a test.
    #[must_use]
    pub fn named(&self, key: &Key) -> Vec<u64> {
        self.named
            .get(&key.l2())
            .map_or_else(Vec::new, |keys| keys.iter().copied().collect())
    }

    /// The key floors the estimator's decay lowered since the last call: the size key, the
    /// floor the summary held before, and the floor to keep, which is `0` when what is
    /// left is no more than the first rung (a floor that low changes no booking). The
    /// decay is the estimator's: a floor halves only once its key has run 256 times and 7
    /// days have passed without a kill or a sample above half of it.
    pub fn take_lowered(&mut self) -> Vec<(u64, u64, u64)> {
        let default = self.default_memory;
        std::mem::take(&mut self.lowered)
            .into_iter()
            .map(|(key, before, after)| (key, before, if after > default { after } else { 0 }))
            .collect()
    }

    /// Keeps `attempt`, the report of `lease`, until the scheduler has said whether it
    /// took the report ([`Learned::finish`]) or the kill it caused is requeued
    /// ([`Learned::requeued`]). `None`: nothing to show for the lease.
    pub fn stage(&mut self, lease: LeaseId, attempt: Option<Attempt>) {
        match attempt {
            Some(attempt) => {
                self.staged.insert(lease, attempt);
            }
            None => self.skip(Skip::NoBasis),
        }
    }

    /// The scheduler gave up `lease` after an own-limit kill and will book `raised` next:
    /// the staged attempt is observed with that, and the rows of the summaries it changed
    /// are returned, for the caller to write after the requeue is settled.
    pub fn requeued(&mut self, lease: LeaseId, raised: u64, bounds: &Bounds) -> Vec<Row> {
        self.forget(lease);
        // A kill is no run to compare: the operation's prediction waits for its next one.
        self.unstage_prediction(lease);
        let Some(mut attempt) = self.staged.remove(&lease) else {
            return Vec::new();
        };
        attempt.run.raised_memory_bytes = Some(raised);
        self.observe_keyed(&attempt, bounds);
        let touched = BTreeSet::from([attempt.key.clone(), attempt.key.l2()]);
        self.rows_of(touched)
    }

    /// The scheduler has answered the report of `lease`: if it took it, the staged
    /// attempt is observed; if not, it is dropped.
    ///
    /// Returns the rows of the summaries an own-limit kill changed (a kill that ended the
    /// operation, with no requeue), for the caller to write at once.
    pub fn finish(&mut self, lease: LeaseId, taken: bool, bounds: &Bounds) -> Vec<Row> {
        self.forget(lease);
        let compared = self.unstage_prediction(lease).filter(|_| taken);
        let attempt = self.staged.remove(&lease).filter(|_| taken);
        if let Some((operation, prediction)) = compared {
            // A report the scheduler took ends the operation, with or without a run to
            // compare (a run that named no instrument has no attempt).
            self.end_prediction(operation, &prediction, attempt.as_ref().map(|a| &a.run));
        }
        let Some(attempt) = attempt else {
            return Vec::new();
        };
        self.observe_keyed(&attempt, bounds);
        if attempt.run.outcome != RunOutcome::OwnLimitKill {
            return Vec::new();
        }
        self.rows_of(BTreeSet::from([attempt.key.clone(), attempt.key.l2()]))
    }

    /// [`Learned::observe`], noting the key floors its decay lowered.
    fn observe_keyed(&mut self, attempt: &Attempt, bounds: &Bounds) {
        let l2 = attempt.key.l2();
        let floor = |learned: &Self| learned.summaries.get(&l2).map_or(0, Summary::oom_floor);
        let before = floor(self);
        self.observe(attempt, bounds);
        let after = floor(self);
        if after < before {
            let named = self.named.get(&l2).into_iter().flatten().copied();
            let lowered: Vec<_> = named.map(|size_key| (size_key, before, after)).collect();
            self.lowered.extend(lowered);
        }
    }

    /// Shows the estimator `attempt`: asks it first what it would have booked, then
    /// teaches it. Returns where the attempt fell, if it was a memory sample.
    pub fn observe(
        &mut self,
        attempt: &Attempt,
        bounds: &Bounds,
    ) -> Option<(ShadowLevel, ShadowBucket)> {
        self.calls += 1;
        let day = self.day();
        let estimate = self
            .summaries
            .estimate(&attempt.key, day, bounds, &Hint::default());
        let seen = self
            .summaries
            .observe(&attempt.key, &attempt.run, day, bounds);
        self.dirty.insert(attempt.key.l2());
        self.dirty.insert(attempt.key.clone());
        let landed = seen.memory.ok().map(|()| bucket(&estimate, &attempt.run));
        if let Some((level, bucket)) = landed {
            self.tally[level as usize][bucket as usize] += 1;
        }
        landed
    }

    /// What the estimator would book for an operation of `key` that named `hint`, as the
    /// text of the `size estimate:` line: the shape key (as the `action usage:` line
    /// writes it), then the answer and the default and floor it is compared with.
    pub fn estimate_text(&mut self, key: &Key, hint: &Hint, bounds: &Bounds) -> String {
        self.calls += 1;
        let day = self.day();
        let floor = self.summaries.key_floor(key, bounds) / kbf_estimator::MIB;
        let default = self.default_memory / kbf_estimator::MIB;
        let shape = key.shape().0;
        match self.summaries.estimate(key, day, bounds, hint) {
            Ok(e) => format!(
                "key={shape:016x} level={} memory_mib={} cpu_millis={} default_mib={default} \
                 floor_mib={floor}",
                level_name(e.level),
                e.memory_bytes / kbf_estimator::MIB,
                e.cpu_millis,
            ),
            Err(why) => {
                format!("key={shape:016x} fallback={why:?} default_mib={default} floor_mib={floor}")
            }
        }
    }

    /// Copies the tally into the scrape.
    pub fn publish(&self, totals: &ShadowTotals) {
        for skip in SKIPS {
            totals.set_skipped(skip as usize, self.skipped(skip));
        }
        for (l, level) in LEVELS.into_iter().enumerate() {
            for (b, bucket) in BUCKETS.into_iter().enumerate() {
                totals.set(level, bucket, self.tally[l][b]);
            }
        }
    }

    /// The count of `level` and `bucket`, for a test.
    #[must_use]
    pub fn tallied(&self, level: ShadowLevel, bucket: ShadowBucket) -> u64 {
        self.tally[level as usize][bucket as usize]
    }

    /// The rows of `keys` as the summaries are now, which are no longer dirty.
    pub fn take_rows(&mut self, keys: BTreeSet<Key>) -> Vec<Row> {
        self.rows_of(keys)
    }

    fn rows_of(&mut self, keys: BTreeSet<Key>) -> Vec<Row> {
        let mut rows = Vec::new();
        for key in keys {
            self.dirty.remove(&key);
            if let Some(summary) = self.summaries.get(&key) {
                rows.push((key, summary.encode()));
            }
        }
        rows
    }

    /// The batch to write behind, if one is due: the dirty keys wait [`FLUSH_EVERY_MS`],
    /// or a full batch does not. The caller puts the keys back with
    /// [`Learned::unwritten`] if the store refuses.
    pub fn due(&mut self) -> Vec<Row> {
        self.batch(false)
    }

    /// [`Learned::due`] without the wait: the batch whatever the time since the last one.
    pub fn due_now(&mut self) -> Vec<Row> {
        self.batch(true)
    }

    fn batch(&mut self, now: bool) -> Vec<Row> {
        let waited = self
            .now
            .as_millis()
            .saturating_sub(self.flushed.as_millis());
        if self.off() || (!now && waited < FLUSH_EVERY_MS && self.dirty.len() < BATCH) {
            return Vec::new();
        }
        self.flushed = self.now;
        let batch = self.dirty.iter().take(BATCH).cloned().collect();
        self.rows_of(batch)
    }

    /// The store did not take `rows`: their keys are written again next time.
    pub fn unwritten(&mut self, rows: &[Row]) {
        self.dirty.extend(rows.iter().map(|(key, _)| key.clone()));
    }

    /// The day to sweep stale rows of the store for, if a sweep is due (an hour after the
    /// last; the first look only starts the hour) or one is still going. Drops the stale
    /// summaries from memory when a new one starts.
    pub fn sweep_due(&mut self) -> Option<Day> {
        if self.off() {
            return None;
        }
        if self.sweeping.is_some() {
            return self.sweeping;
        }
        let due = self
            .swept
            .map(|at| at.as_millis().saturating_add(SWEEP_EVERY_MS));
        self.swept = self.swept.or(Some(self.now));
        if due.is_none_or(|due| self.now.as_millis() < due) {
            return None;
        }
        self.swept = Some(self.now);
        let day = self.day();
        self.summaries.expire(day);
        self.sweeping = Some(day);
        Some(day)
    }

    /// The sweep for [`Learned::sweep_due`]'s day: `more` it has left for the next tick.
    pub fn swept(&mut self, more: bool) {
        if !more {
            self.sweeping = None;
        }
    }
}

const SKIPS: [Skip; 5] = [
    Skip::Kind,
    Skip::NoOs,
    Skip::NoBooking,
    Skip::NoBasis,
    Skip::NoSubject,
];
const LEVELS: [ShadowLevel; 3] = [ShadowLevel::L1, ShadowLevel::L2, ShadowLevel::None];
const BUCKETS: [ShadowBucket; 4] = [
    ShadowBucket::Covered,
    ShadowBucket::WouldOom,
    ShadowBucket::Censored,
    ShadowBucket::Cold,
];

const fn level_name(level: Level) -> &'static str {
    match level {
        Level::L1 => "l1",
        Level::L2 => "l2",
    }
}

/// Where a memory sample fell against `estimate`, the estimator's answer from before it.
fn bucket(estimate: &Result<Estimate, Fallback>, run: &Run) -> (ShadowLevel, ShadowBucket) {
    let Ok(e) = estimate else {
        return (ShadowLevel::None, ShadowBucket::Cold);
    };
    let level = match e.level {
        Level::L1 => ShadowLevel::L1,
        Level::L2 => ShadowLevel::L2,
    };
    let limit = memory_max(e.memory_bytes).unwrap_or(0);
    let own = memory_max(run.booked_memory_bytes).unwrap_or(0);
    let killed = run.outcome == RunOutcome::OwnLimitKill;
    let used = run.peak_memory_bytes.unwrap_or(0);
    // A kill's peak is where it was cut off.
    let peak = if killed { used.max(own) } else { used };
    let lower_bound = killed || peak.saturating_mul(20) >= own.saturating_mul(19);
    let bucket = if peak > limit {
        ShadowBucket::WouldOom
    } else if lower_bound || peak.saturating_mul(20) > limit.saturating_mul(19) {
        ShadowBucket::Censored
    } else {
        ShadowBucket::Covered
    };
    (level, bucket)
}

/// FNV-1a: a stable hash of a configuration's text.
#[must_use]
pub fn cfg_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// The instrument a node of `os_family` measures with.
#[must_use]
pub fn basis_of_os(os_family: &str) -> Option<Basis> {
    match os_family {
        "linux" => Some(Basis::Cgroup),
        "macos" => Some(Basis::MacFootprint),
        _ => None,
    }
}

/// The estimator's key for an action of `instance` described by `subject`, to run on
/// `platform` and be measured with `basis`; why not when the request's `needs` do not pin
/// the OS, or its subject is unknown.
pub fn key_of(
    instance: &str,
    platform: &Platform,
    basis: Basis,
    subject: &Subject,
) -> Result<Key, Skip> {
    if platform.os_family == "-" {
        return Err(Skip::NoOs);
    }
    if subject.key == *UNKNOWN_SUBJECT {
        return Err(Skip::NoSubject);
    }
    let class = if platform.isa == "-" {
        platform.os_family.to_owned()
    } else {
        format!("{}/{}", platform.os_family, platform.isa)
    };
    let cfg = subject
        .cfg
        .as_deref()
        .filter(|c| !c.is_empty() && *c != "-")
        .map(|c| Cfg(cfg_hash(c)));
    Ok(Key::new(
        Instance::new(instance),
        PlatformClass::new(class),
        basis,
        Shape(u64::from_be_bytes(subject.key.bytes())),
        cfg,
    ))
}

/// The shape key of an action described by nothing: what an operation restored from a
/// row that kept no subject is given.
static UNKNOWN_SUBJECT: LazyLock<ShapeKey> =
    LazyLock::new(|| Subject::of(Described::default()).0.key);

/// The run of an own-limit kill that carried no usage: it died at its limit.
#[must_use]
pub fn kill_run(booked: u64) -> Run {
    Run {
        outcome: RunOutcome::OwnLimitKill,
        wall_us: 0,
        peak_memory_bytes: None,
        booked_memory_bytes: booked,
        raised_memory_bytes: None,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us: 0,
        node_cpu_pressure_permille: None,
    }
}

/// The estimator's view of a finished attempt: how it ended, what its usage block says
/// (`None`: it carried none), and what it booked. `None` when the run names no
/// instrument, which is never a sample.
#[must_use]
pub fn run_of(outcome: Outcome, usage: Option<&RunUsage>, booked: u64) -> Option<(Basis, Run)> {
    let usage = usage?;
    let outcome = match outcome {
        Outcome::Completed { .. } => RunOutcome::Success,
        Outcome::Failed(Failure::OutOfMemory) => RunOutcome::OwnLimitKill,
        Outcome::Failed(Failure::NodeMemoryPressure) => RunOutcome::NodePressureKill,
        Outcome::Failed(Failure::Lost(_)) => RunOutcome::Lost,
        Outcome::Failed(_) => RunOutcome::Failed,
    };
    let (basis, wall_us, peak, cpu_us) = match usage.basis {
        UsageBasis::Cgroup => (
            Basis::Cgroup,
            usage.wall_micros,
            usage.memory_peak_bytes,
            usage.cpu_usage_micros.unwrap_or(0),
        ),
        UsageBasis::MacFootprint => {
            let f = usage.footprint.unwrap_or_default();
            let cpu = f
                .cpu_user_micros
                .unwrap_or(0)
                .saturating_add(f.cpu_system_micros.unwrap_or(0));
            (
                Basis::MacFootprint,
                f.wall_micros.unwrap_or(usage.wall_micros),
                f.peak_footprint_bytes,
                cpu,
            )
        }
        UsageBasis::Unspecified => return None,
    };
    let run = Run {
        outcome,
        wall_us,
        peak_memory_bytes: peak,
        booked_memory_bytes: booked,
        raised_memory_bytes: None,
        limit_hits: usage.memory_events_max.unwrap_or(0),
        swap_peak_bytes: usage.memory_swap_peak_bytes.unwrap_or(0),
        cpu_usage_us: cpu_us,
        node_cpu_pressure_permille: usage
            .node_cpu_pressure_centi
            .map(|c| u16::try_from(c / 10).unwrap_or(u16::MAX)),
    };
    Some((basis, run))
}

/// What the client named for the request: an axis it sized is a hint the estimate never
/// goes below. `memory` and `cpu_millis` are the request's own.
#[must_use]
pub fn hint_of(evidence: &Evidence, memory: u64, cpu_millis: u64) -> Hint {
    let named = |source: BookSource| source == BookSource::Hint;
    Hint {
        memory_bytes: named(evidence.memory_source).then_some(memory),
        cpu_millis: named(evidence.cpu_source)
            .then(|| u32::try_from(cpu_millis).unwrap_or(u32::MAX)),
    }
}

#[cfg(test)]
mod tests;
