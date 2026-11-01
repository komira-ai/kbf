//! What the farm on the js core learns from the runs it hears of (`--usage-profiles record`):
//! the hub of `js-usage` over this server's state store, fed from the heartbeats and the
//! results the farm takes, after the core has let go of its lock.
//!
//! The farm tells the hub three things, and nothing here changes where work goes (the curves
//! are not given to the core):
//!
//! - **a profile name for each action** ([`Usage::note_action`]): when an operation is
//!   submitted or taken up from the store, the estimator's key of its action, as the profile
//!   name its runs are kept under;
//! - **a beat** ([`Usage::beat`]): the load a daemon's heartbeat carried, and the results the
//!   farm accepted from that daemon since its last beat. The hub counts the grace of a run in
//!   beats of its host (the last partial minute of a lease rides on a later beat), so a result
//!   waits here for its worker's next beat and is not handed over at once;
//! - **a tick** ([`Usage::tick`]): every [`FLUSH_EVERY_MINUTES`] minutes, on the blocking
//!   pool, and a last flush at shutdown ([`Usage::finish`]).
//!
//! **Runs.** A run is a lease; its id is `<term>.<seq>`. A result the farm accepted binds its
//! run to its action in [`RunKeys`] and waits for the worker's next beat. The hub asks for the
//! key when it reads the result, in that beat. [`Usage::finish_runs`] is the one place a run
//! is forgotten, and every way a run can end goes through it: its result read by a beat (clean
//! or unclean), a result no beat read for a day ([`Usage::tick`]: the worker went away), and a
//! handover ([`Usage::drop_all`]). A run that never had an accepted result (a lease given up,
//! a refused operation) was never bound.
//!
//! **Order.** The profile name of an action is read from the [`ShapeIndex`] when the hub reads
//! the result, so the entry is removed after the beat that carried the result, never when the
//! result is accepted. Two operations of one action in flight share the entry: the first to
//! finish removes it, and the other's result then has no key (counted by the hub as a run
//! without a key) until the action is submitted again.
//!
//! **Never in the lock.** The hub blocks on the store, so it is called from the farm's own
//! tasks ([`hot`](crate::farm::js::hot)) after the core's calls returned, on the blocking
//! pool. A panic in the hub is caught and counted as a fault; a beat or a result is never
//! refused because of usage.

// The behaviour of the methods of `Usage` arrives in the next commit.
#![expect(
    dead_code,
    unused_imports,
    reason = "the behaviour of Usage arrives in the next commit"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use js_usage::{
    AcceptedResult, Beat, BeatLoad, FLUSH_EVERY_MINUTES, Finish, Limits, LoadIn, ProfileError,
    TRACK_IDLE_MINUTES, UsageCounters, UsageHub, UsageSink,
};
use kbf_state::Store;
use kbf_types::sched::usage::NodeLoad;
use kbf_types::{ActionKey, LeaseId, WorkerId};
use kbf_usage::{HostNames, KbfUsageSink, ProfileSink, RunKeys, ShapeIndex, profile_name};

use super::learning::estimator_key;
use crate::action_usage::Subject;
use crate::machine::Platform;

/// The bytes the stored profiles may take.
const BUDGET_BYTES: u64 = 32 << 20;

/// The capacity of the nodes is not given to the hub: every check against it is skipped.
/// The nodes' own readings are judged by the recorder's other rules.
const NO_LIMITS: Limits = Limits {
    threads: 0,
    memory_bytes: 0,
};

/// The id of the run of `lease`.
fn run_id(lease: LeaseId) -> String {
    format!("{}.{}", lease.term, lease.seq)
}

/// A result the farm accepted that no beat has carried to the hub yet.
struct Waiting {
    lease: js_types::LeaseId,
    run_id: String,
    action: ActionKey,
    finish: Finish,
    /// The wall minute it was accepted at.
    minute: u64,
}

/// What the usage side holds and has counted, for `/metrics` and the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageStats {
    /// What the hub counted.
    pub(crate) counters: UsageCounters,
    /// Runs a profile declined (the hub counts them learned: the call succeeded).
    pub(crate) declined: u64,
    /// Runs that could not be bound because the table was full: never learned.
    pub(crate) binds_refused: u64,
    /// The keys with a stored profile.
    pub(crate) keys: usize,
    /// The bytes of the stored profiles.
    pub(crate) bytes: u64,
    /// The runs bound to an action now.
    pub(crate) held_runs: usize,
    /// The accepted results no beat has carried yet.
    pub(crate) waiting: usize,
    /// How many ticks ran.
    pub(crate) ticks: u64,
}

impl UsageStats {
    /// Appends the usage families to `out`.
    pub(crate) fn render(&self, out: &mut String) {
        let counters = &self.counters;
        let rows: [(&str, &str, &str, u64); 12] = [
            (
                "kbf_usage_runs_learned_total",
                "counter",
                "Runs the usage hub handed to the profile store",
                counters.runs_learned,
            ),
            (
                "kbf_usage_runs_declined_total",
                "counter",
                "Runs the profile store declined: too few minutes",
                self.declined,
            ),
            (
                "kbf_usage_runs_without_key_total",
                "counter",
                "Clean runs whose action had no profile name",
                counters.runs_without_key,
            ),
            (
                "kbf_usage_runs_without_series_total",
                "counter",
                "Clean runs with no minutes on file",
                counters.runs_without_series,
            ),
            (
                "kbf_usage_binds_refused_total",
                "counter",
                "Runs not followed because the table of runs was full",
                self.binds_refused,
            ),
            (
                "kbf_usage_loads_rejected_total",
                "counter",
                "Heartbeat loads the usage side discarded as invalid",
                counters.loads_rejected,
            ),
            (
                "kbf_usage_minutes_unheld_total",
                "counter",
                "Lease minutes from a host that does not hold the lease",
                counters.minutes_unheld,
            ),
            (
                "kbf_usage_faults_total",
                "counter",
                "Store calls that failed and beats whose usage work panicked",
                counters.faults,
            ),
            (
                "kbf_usage_ticks_total",
                "counter",
                "Ticks of the usage side",
                self.ticks,
            ),
            (
                "kbf_usage_profile_keys",
                "gauge",
                "Keys with a stored profile",
                self.keys as u64,
            ),
            (
                "kbf_usage_profile_bytes",
                "gauge",
                "Bytes of the stored profiles",
                self.bytes,
            ),
            (
                "kbf_usage_runs_held",
                "gauge",
                "Runs bound to an action, waiting to be read",
                self.held_runs as u64,
            ),
        ];
        for (name, kind, help, value) in rows {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
            );
        }
    }
}

/// The farm's usage side: see the [module](self).
pub(crate) struct Usage {
    hub: UsageHub,
    sink: Arc<ProfileSink>,
    seam: Arc<KbfUsageSink>,
    shapes: Arc<ShapeIndex>,
    keys: Arc<RunKeys>,
    names: HostNames,
    waiting: Mutex<BTreeMap<WorkerId, Vec<Waiting>>>,
    /// The wall minute from which [`Usage::tick`] runs again.
    next_tick: AtomicU64,
    ticks: AtomicU64,
}

impl Usage {
    /// The usage side over `store`, made at wall minute `now_minute`.
    ///
    /// # Errors
    /// The profiles in the store cannot be read.
    pub(crate) fn open(store: Arc<dyn Store>, now_minute: u64) -> Result<Self, ProfileError> {
        Self::open_through(store, now_minute, |seam| seam)
    }

    /// [`Usage::open`], with the hub calling the sink `wrap` makes from the sink over the store.
    pub(crate) fn open_through(
        store: Arc<dyn Store>,
        now_minute: u64,
        wrap: impl FnOnce(Arc<KbfUsageSink>) -> Arc<dyn UsageSink>,
    ) -> Result<Self, ProfileError> {
        let shapes = Arc::new(ShapeIndex::new());
        let sink = Arc::new(ProfileSink::open(
            store,
            &shapes,
            BUDGET_BYTES,
            now_minute / 1440,
        )?);
        let seam = Arc::new(KbfUsageSink::new(Arc::clone(&sink)));
        let keys = Arc::new(RunKeys::new(Arc::clone(&shapes)));
        let hub = UsageHub::new(wrap(Arc::clone(&seam)), keys.clone(), NO_LIMITS, now_minute);
        Ok(Self {
            hub,
            sink,
            seam,
            shapes,
            keys,
            names: HostNames::new(),
            waiting: Mutex::default(),
            next_tick: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<WorkerId, Vec<Waiting>>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the profile name of `action`, which an operation was submitted or taken up
    /// for. An action the estimator has no key for (its request does not pin the OS, or its
    /// subject is unknown) has none, and its runs are not learned.
    pub(crate) fn note_action(&self, action: &ActionKey, platform: &Platform, subject: &Subject) {
        let _ = (action, platform, subject);
    }

    /// Takes note that the farm accepted the result of `lease`, which ran `action`, from
    /// `worker`, at wall minute `now_minute`: the run is bound, and the hub hears of it on
    /// the worker's next beat. A run that cannot be bound (the table is full) is counted
    /// ([`UsageStats::binds_refused`]) and not followed.
    pub(crate) fn accepted(
        &self,
        worker: &WorkerId,
        lease: LeaseId,
        action: ActionKey,
        clean: bool,
        now_minute: u64,
    ) {
        let _ = (worker, lease, action, clean, now_minute);
    }

    /// The one place a run is forgotten: its key, and the profile name of its action. Called
    /// once a beat has read its result, or when no beat will.
    fn finish_runs(&self, ended: Vec<Waiting>) {
        for run in ended {
            self.keys.forget(&run.run_id);
            self.shapes.remove(&run.action);
        }
    }

    /// What a heartbeat of `worker` tells the hub, at wall minute `now_minute`: its `load`
    /// (`held` are the leases in it the farm holds for the worker), and the results accepted
    /// from it since its last beat. Blocks on the store.
    pub(crate) fn beat_blocking(
        &self,
        worker: &WorkerId,
        epoch: u64,
        load: Option<&NodeLoad>,
        held: &BTreeSet<js_types::LeaseId>,
        now_minute: u64,
    ) {
        let _ = (worker, epoch, load, held, now_minute);
    }

    /// [`Usage::beat_blocking`] on the blocking pool.
    pub(crate) async fn beat(
        self: &Arc<Self>,
        worker: WorkerId,
        epoch: u64,
        load: Option<NodeLoad>,
        held: BTreeSet<js_types::LeaseId>,
        now_minute: u64,
    ) {
        let usage = Arc::clone(self);
        let _joined = tokio::task::spawn_blocking(move || {
            usage.beat_blocking(&worker, epoch, load.as_ref(), &held, now_minute);
        })
        .await;
    }

    /// The tick's work at wall minute `now_minute`: the hub's own (a flush every
    /// [`FLUSH_EVERY_MINUTES`] minutes, which also tells the curves the day), and the runs
    /// whose worker has not beaten for a day are let go. Blocks on the store.
    pub(crate) fn tick_blocking(&self, now_minute: u64) {
        let _ = now_minute;
    }

    /// [`Usage::tick_blocking`] on the blocking pool, once for each [`FLUSH_EVERY_MINUTES`]
    /// wall minutes: a call before the next is due does nothing.
    pub(crate) async fn tick(self: &Arc<Self>, now_minute: u64) {
        if now_minute < self.next_tick.load(Ordering::Relaxed) {
            return;
        }
        self.next_tick
            .store(now_minute + FLUSH_EVERY_MINUTES, Ordering::Relaxed);
        let usage = Arc::clone(self);
        let _joined = tokio::task::spawn_blocking(move || usage.tick_blocking(now_minute)).await;
    }

    /// The flush at shutdown, whatever the schedule says; whether the store took it.
    pub(crate) fn finish_blocking(&self, now_minute: u64) -> bool {
        let _ = now_minute;
        true
    }

    /// [`Usage::finish_blocking`] on the blocking pool.
    pub(crate) async fn finish(self: &Arc<Self>, now_minute: u64) -> bool {
        let usage = Arc::clone(self);
        let joined = tokio::task::spawn_blocking(move || usage.finish_blocking(now_minute)).await;
        joined.unwrap_or(false)
    }

    /// Lets go of every result no beat has read: the farm hands its work over, and the
    /// beats of its workers go to another frontend.
    pub(crate) fn drop_all(&self) {}

    /// What is held and counted.
    pub(crate) fn stats(&self) -> UsageStats {
        let sink = self.sink.stats();
        UsageStats {
            counters: self.hub.counters(),
            declined: self.seam.declined(),
            binds_refused: self.keys.refused(),
            keys: sink.keys,
            bytes: sink.bytes,
            held_runs: self.keys.len(),
            waiting: self.lock().values().map(Vec::len).sum(),
            ticks: self.ticks.load(Ordering::Relaxed),
        }
    }

    /// The profile store's sink.
    #[cfg(test)]
    pub(crate) fn sink(&self) -> &Arc<ProfileSink> {
        &self.sink
    }

    /// The profile names of the actions in flight.
    #[cfg(test)]
    pub(crate) fn shapes(&self) -> &Arc<ShapeIndex> {
        &self.shapes
    }
}
